// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

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

use super::chroma_analyze::Rgb;

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
    /// (bytes; `VirtualQuery` rounding to allocation granularity).
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
    #[must_use]
    pub fn name(&self) -> String {
        format!("Global\\{{{}}}", self.guid)
    }
}

/// The security descriptor we put on our objects: DACL granting **Everyone
/// (`WD`) `GenericAll`** — critically including READ. The game's worker maps every
/// object with `MapViewOfFile(FILE_MAP_ALL_ACCESS = 0xf001f)`, which REQUIRES
/// read access; a write-only DACL (the earlier `0x1f0003`, missing the 0x4 read
/// bit) made every neuron object fail to map with `ACCESS_DENIED`, so the worker
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
    Obj { guid: "0F9297E6-E80C-47E4-9A8B-1237E50484B7", kind: Kind::Event, origin: Origin::ClientCreated, note: "server offline: client revokes access, writes None" },
    Obj { guid: "89811F96-91C2-4C19-8E0A-54469F491550", kind: Kind::Event, origin: Origin::ClientCreated, note: "client re-checks access" },
    Obj { guid: "A84AF9C8-EFE0-430D-871C-10DA760C2CCD", kind: Kind::Event, origin: Origin::ClientCreated, note: "server online" },
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

/// The session table ([`D41D8537`]): a ring clients append `{pid, code (1 = init)}` plus a
/// `GetTickCount64` to when they connect. This reads the head and the first slot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SessionTable {
    /// The ring head at `+0x00` (0 = nobody has registered).
    pub active_count: u32,
    /// The first slot's client PID at `+0x08`.
    pub session_id: u32,
    /// The low dword of a registration tick at `+0x10`.
    pub session_handle: u32,
}

/// Parse the session/priority table. `None` if too short or no active session.
#[must_use]
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
#[must_use]
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
#[must_use]
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
/// The event a client pulses after appending to the session table ([`SESSION_TABLE`]).
/// Per-class frame events are [`DeviceClass::frame_event`].
pub const SESSION_TABLE_EVENT: &str = "DA5A60F0-A3C5-4335-A039-BCC6136C61A3";

/// The per-app registration mutex a connecting client creates and *owns* while
/// connected: `Global\<exe>_rz` with the exe name lowercased (e.g.
/// `Global\overwatch_rz`). Enumerating these owned mutexes is how the server
/// learns which apps are live.
#[must_use]
pub fn rz_mutex_name(exe_name: &str) -> String {
    // Strip any path, drop the extension, lowercase, append `_rz`.
    let base = exe_name.rsplit(['\\', '/']).next().unwrap_or(exe_name);
    let stem = base.rsplit_once('.').map_or(base, |(s, _)| s);
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
#[must_use]
pub fn device_section(device_type: u8) -> Option<&'static str> {
    DEVICE_SECTIONS.iter().find(|(t, _)| *t == device_type).map(|(_, g)| *g)
}

// ─────────────────────────── frame codec ───────────────────────────
//
// Every device section is `u32 head | u32 0 | 10 records | per-instance name strings`. `head`
// is the slot the client writes next, so the newest complete record is `head - 1`. A record
// starts with `devmask = (class << 16) | device-code` (0xFFFF = any device of the class), then
// an effect code and a union of every effect's fields at fixed offsets, and ends with the
// u64 `GetTickCount64` of the write. Colour fields are XOR-obfuscated with [`KEYSTREAM`]
// keyed by that tick; effect parameters are plaintext.
//
// Offsets come from RzChromaSDK64 3.37's per-class builders and agree with the independent
// RazerSdkReader layouts; the fixtures under `chroma_shm_data/` pin them to real Overwatch
// bytes for every class Overwatch writes.

/// A Chroma device class: the kind of device a section's records describe.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum DeviceClass {
    Keyboard,
    Mouse,
    Headset,
    Mousepad,
    Keypad,
    /// Razer's "room" channel: 5 virtual LEDs partner devices (and room lights) follow.
    /// LED 0 is the base colour every partner LED takes; 1-4 are optional accents.
    ChromaLink,
}

impl DeviceClass {
    pub const ALL: [DeviceClass; 6] = [
        DeviceClass::Keyboard,
        DeviceClass::Mouse,
        DeviceClass::Headset,
        DeviceClass::Mousepad,
        DeviceClass::Keypad,
        DeviceClass::ChromaLink,
    ];

    /// The class bit neuron's `device_type` bytes use (and records carry in `devmask >> 16`).
    #[must_use]
    pub fn bit(self) -> u8 {
        match self {
            DeviceClass::Keyboard => 0x01,
            DeviceClass::Mouse => 0x02,
            DeviceClass::Headset => 0x04,
            DeviceClass::Mousepad => 0x08,
            DeviceClass::Keypad => 0x10,
            DeviceClass::ChromaLink => 0x80,
        }
    }

    #[must_use]
    pub fn from_bit(bit: u8) -> Option<Self> {
        Self::ALL.into_iter().find(|c| c.bit() == bit)
    }

    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            DeviceClass::Keyboard => "keyboard",
            DeviceClass::Mouse => "mouse",
            DeviceClass::Headset => "headset",
            DeviceClass::Mousepad => "mousepad",
            DeviceClass::Keypad => "keypad",
            DeviceClass::ChromaLink => "room",
        }
    }

    /// The shared section this class's frames live in.
    #[must_use]
    pub fn section(self) -> &'static str {
        match self {
            DeviceClass::Keyboard => "74164FAD-E73C-4FA1-A9AA-70813315ED9C",
            DeviceClass::Mouse => "0DBE78AC-AC93-408F-A27E-8F61EA067B05",
            DeviceClass::Headset => "CDB274E2-C50A-4425-8076-1E71550CBE8A",
            DeviceClass::Mousepad => "17EFA16B-E476-4E43-A98A-3AA837681741",
            DeviceClass::Keypad => "0FFE5A62-387E-4360-95A3-5D8D4075780D",
            DeviceClass::ChromaLink => "8AE08F8C-BE3E-4248-AB01-0B595960EC3E",
        }
    }

    /// The event the client pulses after committing a frame for this class.
    #[must_use]
    pub fn frame_event(self) -> &'static str {
        match self {
            DeviceClass::Keyboard => "45C97C2C-2D50-4F30-B50E-AFBB1CE22E93",
            DeviceClass::Mouse => "0DB0CEFA-C51E-4255-87FB-2D36A0159896",
            DeviceClass::Headset => "9FE422BE-A752-4F67-9EC6-11ED6135478E",
            DeviceClass::Mousepad => "A966C3C0-231A-4BE5-9C90-5E0C80349891",
            DeviceClass::Keypad => "5C49A446-0B97-46CA-BD60-EE5CAF8DDD59",
            DeviceClass::ChromaLink => "4D006319-9569-4E38-B0DF-811AA2DF115F",
        }
    }

    /// The record layout for this class.
    #[must_use]
    pub fn layout(self) -> &'static Layout {
        match self {
            DeviceClass::Keyboard => &KEYBOARD,
            DeviceClass::Mouse => &MOUSE,
            DeviceClass::Headset => &HEADSET,
            DeviceClass::Mousepad => &MOUSEPAD,
            DeviceClass::Keypad => &KEYPAD,
            DeviceClass::ChromaLink => &CHROMA_LINK,
        }
    }
}

/// Where one device class keeps each field in its record (offsets from the record start).
#[derive(Debug)]
pub struct Layout {
    /// Bytes per record.
    pub stride: usize,
    /// The u32 effect code.
    pub effect: usize,
    /// The u64 write tick.
    pub tick: usize,
    /// The XORed static colour.
    pub static_colour: usize,
    /// The XORed custom colour grid: offset, rows, cols.
    pub grid: (usize, u8, u8),
    /// The effect code that means "custom grid" for this class.
    pub grid_code: u32,
    /// Keyboard CUSTOM_KEY planes (colour, key-override), 6x22 each.
    pub key_planes: Option<(usize, usize)>,
    /// Plaintext effect parameters (see [`Effect`]).
    pub params: Params,
}

/// Offsets of the plaintext parameters of the preset effects, where the class has them.
#[derive(Debug, Default)]
pub struct Params {
    pub wave_dir: Option<usize>,
    /// Breathing type (1 one colour, 2 two colours, 3 random).
    pub breath_type: Option<usize>,
    pub colour1: Option<usize>,
    pub colour2: Option<usize>,
    pub blink_colour: Option<usize>,
    pub reactive_colour: Option<usize>,
    pub reactive_duration: Option<usize>,
}

pub static KEYBOARD: Layout = Layout {
    stride: 0xB98,
    effect: 0x04,
    tick: 0xB90,
    static_colour: 0x48,
    grid: (0x4C, 6, 22),
    grid_code: 7,
    key_planes: Some((0x25C, 0x46C)),
    params: Params {
        wave_dir: Some(0x0C),
        breath_type: Some(0x18),
        colour1: Some(0x1C),
        colour2: Some(0x20),
        blink_colour: None,
        reactive_colour: Some(0x28),
        reactive_duration: Some(0x2C),
    },
};

pub static MOUSE: Layout = Layout {
    stride: 0x1C0,
    effect: 0x08,
    tick: 0x1B8,
    static_colour: 0x1A8,
    grid: (0x9C, 9, 7),
    grid_code: 8,
    key_planes: None,
    params: Params {
        wave_dir: Some(0x1B0),
        breath_type: Some(0x10),
        colour1: Some(0x14),
        colour2: Some(0x18),
        blink_colour: Some(0x20),
        reactive_colour: Some(0x19C),
        reactive_duration: Some(0x1A0),
    },
};

pub static HEADSET: Layout = Layout {
    stride: 0x40,
    effect: 0x04,
    tick: 0x38,
    static_colour: 0x30,
    grid: (0x18, 1, 5),
    grid_code: 7,
    key_planes: None,
    params: Params {
        wave_dir: None,
        breath_type: None,
        colour1: Some(0x10),
        colour2: None,
        blink_colour: None,
        reactive_colour: None,
        reactive_duration: None,
    },
};

pub static MOUSEPAD: Layout = Layout {
    stride: 0xC0,
    effect: 0x04,
    tick: 0xB8,
    static_colour: 0x28,
    grid: (0x2C, 1, 15),
    grid_code: 7,
    key_planes: None,
    params: Params {
        wave_dir: Some(0x1C),
        breath_type: Some(0x0C),
        colour1: Some(0x10),
        colour2: Some(0x14),
        blink_colour: None,
        reactive_colour: None,
        reactive_duration: None,
    },
};

pub static KEYPAD: Layout = Layout {
    stride: 0x90,
    effect: 0x04,
    tick: 0x88,
    static_colour: 0x78,
    grid: (0x18, 4, 5),
    grid_code: 7,
    key_planes: None,
    params: Params {
        wave_dir: Some(0x80),
        breath_type: Some(0x0C),
        colour1: Some(0x10),
        colour2: Some(0x14),
        blink_colour: None,
        reactive_colour: Some(0x6C),
        reactive_duration: Some(0x70),
    },
};

/// Chroma Link. The client copies 50 slots but the API defines 5; the rest is game memory.
pub static CHROMA_LINK: Layout = Layout {
    stride: 0xF0,
    effect: 0x04,
    tick: 0xE8,
    static_colour: 0xE4,
    grid: (0x18, 1, 5),
    grid_code: 7,
    key_planes: None,
    params: Params {
        wave_dir: None,
        breath_type: None,
        colour1: None,
        colour2: None,
        blink_colour: None,
        reactive_colour: None,
        reactive_duration: None,
    },
};

/// Records in a device section's ring.
pub const RING_DEPTH: usize = 10;
/// Offset of record 0 in a device section.
pub const RECORD0: usize = 8;

/// The protocol's per-frame XOR keystream (512 bytes). Each colour byte `b` (0..=3) of a
/// word is XORed with `KEYSTREAM[phase + b*0x81]`, where `phase = tick & 0x7f` (clamped so
/// `phase + 3*0x81 < 512`). The key is the same for every colour in a record and rotates
/// with the millisecond clock, so the raw buffer reads as the board strobing while the
/// real, stable picture sits underneath. The table is four 128-byte Razer marketing strings
/// with their nibbles swapped, which is how to find it again if the DLL ever changes it.
///
/// Indexing: `phase` comes only from [`frame_phase`], which masks the tick to `0..=127` and
/// clamps the top three values down to `122..=124`; the channel multiplier is a literal
/// `0..=3` at every read site ([`xor_colour`], and the key-plane flag byte in
/// [`decode_device`]). The worst index is `124 + 3*0x81 = 511`, the table's last byte; the
/// clamp exists for exactly that fourth byte.
pub const KEYSTREAM: &[u8; 512] = include_bytes!("chroma_shm_data/keystream.bin");
const KEYSTREAM_CHANNEL_STRIDE: usize = 0x81;

/// `tick & 0x7f`, clamped the way the writer clamps it (`if 0x80 - phase < 4 { phase -= 3 }`).
fn frame_phase(tick: u64) -> usize {
    let mut phase = (tick & 0x7f) as usize;
    if 0x80 - phase < 4 {
        phase -= 3;
    }
    phase
}

fn u32_at(b: &[u8], off: usize) -> Option<u32> {
    b.get(off..off + 4).map(|s| u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
}

fn u64_at(b: &[u8], off: usize) -> Option<u64> {
    b.get(off..off + 8).map(|s| u64::from_le_bytes([s[0], s[1], s[2], s[3], s[4], s[5], s[6], s[7]]))
}

/// A plaintext `COLORREF` (`0x00BBGGRR`) as `(r, g, b)`.
fn colorref(b: &[u8], off: usize) -> Option<Rgb> {
    b.get(off..off + 3).map(|s| (s[0], s[1], s[2]))
}

/// Decode one XORed colour word at `off`.
fn xor_colour(b: &[u8], off: usize, phase: usize) -> Option<Rgb> {
    let s = b.get(off..off + 3)?;
    Some((
        s[0] ^ KEYSTREAM[phase],
        s[1] ^ KEYSTREAM[phase + KEYSTREAM_CHANNEL_STRIDE],
        s[2] ^ KEYSTREAM[phase + 2 * KEYSTREAM_CHANNEL_STRIDE],
    ))
}

/// What a record asks the device to show. Custom grids arrive as pixels; the preset effects
/// arrive as parameters and are rendered by [`DeviceFrame::cells_at`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Effect {
    /// Lighting off (also what a client writes when it loses access).
    None,
    Static,
    Custom,
    /// Keyboard CUSTOM_KEY / CUSTOM2: a colour grid with per-key overrides.
    CustomKey,
    Wave { reverse: bool },
    Spectrum,
    Breathing { colours: Option<(Rgb, Rgb)> },
    Blinking(Rgb),
    Reactive(Rgb),
    /// An effect code this decoder doesn't model (Init, Suspend, …), passed through.
    Other(u32),
}

impl Effect {
    /// A short lower-case name.
    #[must_use]
    pub fn name(&self) -> &'static str {
        match self {
            Effect::None => "off",
            Effect::Static => "static",
            Effect::Custom => "custom",
            Effect::CustomKey => "custom keys",
            Effect::Wave { .. } => "wave",
            Effect::Spectrum => "spectrum",
            Effect::Breathing { .. } => "breathing",
            Effect::Blinking(_) => "blinking",
            Effect::Reactive(_) => "reactive",
            Effect::Other(_) => "other",
        }
    }
}

/// The internal effect code (shared by every class) as a readable name.
#[must_use]
pub fn effect_name(code: u32) -> &'static str {
    match code {
        0 => "None",
        1 => "Wave",
        2 => "Spectrum",
        3 => "Breathing",
        4 => "Blinking",
        5 => "Reactive",
        6 => "Static",
        7 => "Custom",
        8 => "CustomKey",
        9 => "Init",
        10 => "Uninit",
        11 => "Default",
        12 => "Starlight",
        13 => "Suspend",
        14 => "Resume",
        16 => "Active",
        17 => "Visualizer",
        _ => "Unknown",
    }
}

/// One device class's newest frame.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeviceFrame {
    pub class: DeviceClass,
    pub effect: Effect,
    /// The raw effect code.
    pub effect_code: u32,
    /// `devmask & 0xFFFF`: 0xFFFF for "every device of the class", else a device id.
    pub device: u16,
    /// The write's `GetTickCount64`, milliseconds since boot.
    pub tick_ms: u64,
    pub rows: u8,
    pub cols: u8,
    /// Row-major colours for pixel effects (Custom, CustomKey, Static); empty for the preset
    /// effects, which [`cells_at`](Self::cells_at) renders.
    pub cells: Vec<Rgb>,
}

impl DeviceFrame {
    /// LEDs in the class grid.
    #[must_use]
    pub fn len(&self) -> usize {
        usize::from(self.rows) * usize::from(self.cols)
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// True when every LED holds one colour (Static, or a solid Custom frame).
    #[must_use]
    pub fn uniform(&self) -> Option<Rgb> {
        let first = *self.cells.first()?;
        self.cells.iter().all(|&c| c == first).then_some(first)
    }

    /// The colours to show `t_ms` into a clock the caller keeps. Pixel effects return their
    /// cells; preset effects are rendered: neuron is the server, so it draws what Razer's
    /// service would. Reactive needs key presses the server doesn't see, so it shows dark.
    #[must_use]
    pub fn cells_at(&self, t_ms: u64) -> Vec<Rgb> {
        if !self.cells.is_empty() {
            return self.cells.clone();
        }
        let n = self.len();
        let t = t_ms as f32 / 1000.0;
        match self.effect {
            Effect::Spectrum => vec![hue_rgb(t / 6.0); n],
            Effect::Wave { reverse } => {
                let cols = f32::from(self.cols.max(1));
                (0..n)
                    .map(|i| {
                        let col = (i % usize::from(self.cols.max(1))) as f32 / cols;
                        let x = if reverse { 1.0 - col } else { col };
                        hue_rgb(x - t / 2.0)
                    })
                    .collect()
            }
            Effect::Breathing { colours } => {
                // Four seconds a breath; alternating colours breathe in turn.
                let cycle = (t / 4.0).floor();
                let level = 0.5 - 0.5 * (std::f32::consts::TAU * t / 4.0).cos();
                let base = match colours {
                    Some((a, b)) => {
                        if (cycle as u64).is_multiple_of(2) {
                            a
                        } else {
                            b
                        }
                    }
                    None => hue_rgb(cycle * 0.37),
                };
                vec![scale(base, level); n]
            }
            Effect::Blinking(c) => vec![if (t_ms / 500).is_multiple_of(2) { c } else { (0, 0, 0) }; n],
            _ => vec![(0, 0, 0); n],
        }
    }
}

fn scale(c: Rgb, k: f32) -> Rgb {
    let s = |v: u8| (f32::from(v) * k).round().clamp(0.0, 255.0) as u8;
    (s(c.0), s(c.1), s(c.2))
}

/// A fully saturated colour at hue `h` turns (any real; wraps).
fn hue_rgb(h: f32) -> Rgb {
    let h = h.rem_euclid(1.0) * 6.0;
    let x = 1.0 - (h % 2.0 - 1.0).abs();
    let (r, g, b) = match h as u32 {
        0 => (1.0, x, 0.0),
        1 => (x, 1.0, 0.0),
        2 => (0.0, 1.0, x),
        3 => (0.0, x, 1.0),
        4 => (x, 0.0, 1.0),
        _ => (1.0, 0.0, x),
    };
    let to = |v: f32| (v * 255.0).round() as u8;
    (to(r), to(g), to(b))
}

/// Map a class frame onto a physical device with `leds` LEDs. The keyboard grid maps cell i
/// to LED i (verified on a BlackWidow). Another class maps straight when the counts agree and
/// fills with a solid colour when the frame is one; otherwise the frame's average colour
/// fills the device, since which grid cell sits over which physical zone isn't known.
#[must_use]
pub fn fit_cells(class: DeviceClass, cells: &[Rgb], leds: usize) -> Vec<Rgb> {
    if cells.is_empty() || leds == 0 {
        return vec![(0, 0, 0); leds];
    }
    if class == DeviceClass::Keyboard || cells.len() == leds {
        let mut out: Vec<Rgb> = cells.iter().copied().take(leds).collect();
        out.resize(leds, (0, 0, 0));
        return out;
    }
    let lit: Vec<&Rgb> = cells.iter().filter(|c| (c.0 | c.1 | c.2) != 0).collect();
    if lit.is_empty() {
        return vec![(0, 0, 0); leds];
    }
    let n = lit.len() as u32;
    let sum = lit.iter().fold((0u32, 0u32, 0u32), |a, c| (a.0 + u32::from(c.0), a.1 + u32::from(c.1), a.2 + u32::from(c.2)));
    let avg = ((sum.0 / n) as u8, (sum.1 / n) as u8, (sum.2 / n) as u8);
    vec![avg; leds]
}

/// Byte offset of the newest complete record (`head - 1`), or `None` if the section is too
/// short to hold the ring.
#[must_use]
pub fn newest_record(section: &[u8], class: DeviceClass) -> Option<usize> {
    let stride = class.layout().stride;
    if section.len() < RECORD0 + RING_DEPTH * stride {
        return None;
    }
    let head = u32_at(section, 0)? as usize % RING_DEPTH;
    Some(RECORD0 + ((head + RING_DEPTH - 1) % RING_DEPTH) * stride)
}

/// Decode the newest frame a section holds for `class`. `None` when the section is empty,
/// torn, or its newest record belongs to another class.
#[must_use]
pub fn decode_device(section: &[u8], class: DeviceClass) -> Option<DeviceFrame> {
    let l = class.layout();
    let rec = &section[newest_record(section, class)?..][..l.stride];
    let devmask = u32_at(rec, 0)?;
    if (devmask >> 16) as u8 != class.bit() {
        return None;
    }
    let effect_code = u32_at(rec, l.effect)?;
    let tick_ms = u64_at(rec, l.tick)?;
    let phase = frame_phase(tick_ms);
    let (grid, rows, cols) = l.grid;
    let n = usize::from(rows) * usize::from(cols);
    let p = &l.params;
    let param_colour = |off: Option<usize>| off.and_then(|o| colorref(rec, o));

    let mut cells = Vec::new();
    let effect = match effect_code {
        0 => Effect::None,
        6 => {
            cells = vec![xor_colour(rec, l.static_colour, phase)?; n];
            Effect::Static
        }
        c if c == l.grid_code => {
            cells = (0..n).map(|i| xor_colour(rec, grid + 4 * i, phase)).collect::<Option<_>>()?;
            Effect::Custom
        }
        8 if l.key_planes.is_some() => {
            let (colour, key) = l.key_planes?;
            cells = (0..n)
                .map(|i| {
                    let over = rec.get(key + 4 * i + 3).map(|b| b ^ KEYSTREAM[phase + 3 * KEYSTREAM_CHANNEL_STRIDE]);
                    if over.is_some_and(|b| b & 0x01 != 0) {
                        xor_colour(rec, key + 4 * i, phase)
                    } else {
                        xor_colour(rec, colour + 4 * i, phase)
                    }
                })
                .collect::<Option<_>>()?;
            Effect::CustomKey
        }
        1 => Effect::Wave { reverse: p.wave_dir.and_then(|o| u32_at(rec, o)).is_some_and(|d| d == 2 || d == 4) },
        2 => Effect::Spectrum,
        3 => {
            let kind = p.breath_type.and_then(|o| u32_at(rec, o)).unwrap_or(1);
            let c1 = param_colour(p.colour1);
            let c2 = param_colour(p.colour2);
            Effect::Breathing {
                colours: match (kind, c1, c2) {
                    (3, _, _) => None,
                    (2, Some(a), Some(b)) => Some((a, b)),
                    (_, Some(a), _) => Some((a, a)),
                    _ => None,
                },
            }
        }
        4 => Effect::Blinking(param_colour(p.blink_colour).unwrap_or((255, 255, 255))),
        5 => Effect::Reactive(param_colour(p.reactive_colour).unwrap_or((255, 255, 255))),
        other => Effect::Other(other),
    };
    Some(DeviceFrame {
        class,
        effect,
        effect_code,
        device: (devmask & 0xFFFF) as u16,
        tick_ms,
        rows,
        cols,
        cells,
    })
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
#[must_use]
pub fn sections() -> Vec<(&'static str, usize)> {
    let mut v: Vec<(&'static str, usize)> = OBJECTS
        .iter()
        .filter_map(|o| match o.kind {
            Kind::Section(n) if o.origin == Origin::ServerCreated => Some((o.guid, n)),
            _ => None,
        })
        .collect();
    v.sort_by_key(|entry| std::cmp::Reverse(entry.1));
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
    use super::{SECURITY_SDDL, OBJECTS, Origin, Kind, APP_REGISTRY, device_section, decode_device, fit_cells, DeviceClass, DeviceFrame, Effect, AppEntry, parse_app_registry, SessionTable, SESSION_TABLE, parse_session_table, rz_mutex_name, APP_REGISTRY_RECORD0, client_objects};
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

    /// Create one of the server's named mutexes, without taking initial ownership.
    ///
    /// What the capture actually established about these is existence: they are present while the
    /// vendor server runs and absent when it is stopped (the running-vs-stopped object diff). A
    /// diff cannot observe ownership, so ownership is not a fact we have.
    ///
    /// Ownership is also not what a liveness probe reads. `OpenMutexW` returning a handle is the
    /// probe — that is exactly how [`ShmServer::mask_worn`] checks for a live server — and it
    /// succeeds on an unowned mutex. Taking ownership adds nothing to that signal and takes
    /// something away: a Win32 mutex is owned by a thread, so another process waiting on it
    /// blocks until the owner releases. This server never releases (there is no `ReleaseMutex`
    /// anywhere in the crate), so an owned mutex is an indefinite block for any client that waits
    /// rather than probes. `wear_mask`'s arbitration mutexes already pass 0 for the same reason.
    fn create_named_mutex(sa: *const SECURITY_ATTRIBUTES, name: &[u16]) -> HANDLE {
        const NOT_INITIAL_OWNER: i32 = 0;
        unsafe { CreateMutexW(sa, NOT_INITIAL_OWNER, name.as_ptr()) }
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
                    sddl.as_ptr(), 1, &raw mut psd, std::ptr::null_mut(),
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
            &raw const self.1
        }
    }
    impl Drop for EveryoneSa {
        fn drop(&mut self) {
            if !self.0.is_null() {
                unsafe { LocalFree(self.0.cast()) };
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

    impl Drop for MappedSection {
        fn drop(&mut self) {
            unsafe {
                UnmapViewOfFile(windows_sys::Win32::System::Memory::MEMORY_MAPPED_VIEW_ADDRESS {
                    Value: self.view.cast::<core::ffi::c_void>(),
                });
                CloseHandle(self.mapping);
            }
        }
    }

    /// Holds the fixed Chroma sections open so a limited tray can adopt them. The broker only
    /// creates sections; the tray still owns arbitration, decoding and device output.
    pub struct SectionSeed {
        mappings: Vec<HANDLE>,
        security_attributes: EveryoneSa,
    }

    impl SectionSeed {
        pub fn create() -> Result<Self, CreateError> {
            if mask_worn() {
                return Err(CreateError::AlreadyServing);
            }
            Self::create_named(super::sections().into_iter().map(|(guid, size)| {
                (format!("Global\\{{{guid}}}"), size)
            }))
        }

        fn create_named(names: impl IntoIterator<Item = (String, usize)>) -> Result<Self, CreateError> {
            let sa = EveryoneSa::new()?;
            let mut seed = Self { mappings: Vec::new(), security_attributes: sa };
            for (name, size) in names {
                let name = wide(&name);
                // SAFETY: the name and exact size come only from the fixed, audited object map.
                let mapping = unsafe {
                    CreateFileMappingW(
                        INVALID_HANDLE_VALUE, seed.security_attributes.ptr(), PAGE_READWRITE,
                        0, size as u32, name.as_ptr(),
                    )
                };
                if mapping.is_null() {
                    return Err(io::Error::last_os_error().into());
                }
                // An existing mapping may be smaller than the capture. Mapping the requested
                // length fails in that case, before the tray can report native service as ready.
                let view = unsafe { MapViewOfFile(mapping, FILE_MAP_ALL_ACCESS, 0, 0, size) };
                if view.Value.is_null() {
                    let err = io::Error::last_os_error();
                    unsafe { CloseHandle(mapping) };
                    return Err(err.into());
                }
                unsafe { UnmapViewOfFile(view) };
                seed.mappings.push(mapping);
            }
            Ok(seed)
        }

        #[must_use]
        pub fn section_count(&self) -> usize {
            self.mappings.len()
        }
    }

    impl Drop for SectionSeed {
        fn drop(&mut self) {
            for mapping in self.mappings.drain(..) {
                unsafe { CloseHandle(mapping) };
            }
        }
    }

    /// Cheap readiness probe for a tray that started before the protected broker. It opens
    /// existing mappings only; no privilege, section creation or device I/O is involved.
    #[must_use]
    pub fn seeded_sections_present() -> bool {
        for (guid, _) in super::sections() {
            let name = wide(&format!("Global\\{{{guid}}}"));
            let mapping = unsafe { OpenFileMappingW(FILE_MAP_ALL_ACCESS, 0, name.as_ptr()) };
            if mapping.is_null() {
                return false;
            }
            unsafe { CloseHandle(mapping) };
        }
        true
    }

    /// Why [`ShmServer::create`] declined.
    #[derive(Debug)]
    pub enum CreateError {
        /// The named objects already exist — the real Razer server is running.
        /// Stand down and let it serve (the `OpenRGB` "port busy" rule).
        AlreadyServing,
        /// An OS error (usually: not elevated — `Global\` needs `SeCreateGlobal`).
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
        handles: Vec<SendHandle>, // events + mutexes we created (kept alive)
        _sa: Option<EveryoneSa>,
        mask: Option<MaskGuard>, // the arbitration mask (create-mode only)
        /// Per device class, a frame to paint INSTEAD of the game's while it is set — the
        /// Chroma lab's "hide this effect": it holds the at-rest picture over an effect the user
        /// muted. Written by the lab tap, read by [`ChromaShmLayer::render`].
        holds: std::sync::Mutex<Vec<(u8, Vec<(u8, u8, u8)>)>>,
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
                            guid: o.guid, mapping, view: view.Value.cast::<u8>(), size,
                        });
                    }
                    Kind::Event => {
                        let h = unsafe { CreateEventW(sa.ptr(), 1, 0, name.as_ptr()) };
                        if h.is_null() { return Err(io::Error::last_os_error().into()); }
                        handles.push(SendHandle(h));
                    }
                    Kind::Mutex => {
                        // Exists for the server's lifetime (that is the liveness signal), unowned
                        // so a waiting client is never blocked on us — see `create_named_mutex`.
                        let h = create_named_mutex(sa.ptr(), &name);
                        if h.is_null() { return Err(io::Error::last_os_error().into()); }
                        handles.push(SendHandle(h));
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
                    .map_or((0, 0), |s| (s.view as usize, s.size))
            };
            let appreg = find_sec(APP_REGISTRY);
            let sessinfo = find_sec(SESSION_INFO);
            let keyboard = device_section(0x01).map_or((0, 0), find_sec);
            // `wear_mask` can decline (`None`) if another thread in THIS process won the
            // `mask_worn()` race above and already claimed the single in-process arbiter slot
            // (see `claim_mask_slot`) — the same "a live server owns arbitration" signal as the
            // early check, just caught after the TOCTOU window instead of before it. Treat it
            // identically: stand down rather than let two `chroma-arbiter` threads write the
            // same shared pages.
            let mask = wear_mask(&sa, appreg, sessinfo, keyboard)?;
            Ok(ShmServer { sections, handles, _sa: Some(sa), mask: Some(mask), holds: std::sync::Mutex::default() })
        }

        /// Attach to an ALREADY-RUNNING Chroma server's objects (Razer's real
        /// server) instead of creating them — the **read-alongside** role. neuron
        /// does not serve or handshake; the live server ingests the game's Chroma,
        /// and neuron opens the same device buffers and mirrors them onto the
        /// hardware like any other effect. Opens each section for WRITE, because
        /// the Everyone DACL grants `0x1f0003` (`QUERY|MAP_WRITE`) but not `MAP_READ`;
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
                        guid: o.guid, mapping, view: view.Value.cast::<u8>(), size,
                    });
                }
            }
            if sections.is_empty() {
                return Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    "no Chroma server objects to open — is Razer's Chroma server running?",
                ));
            }
            Ok(ShmServer { sections, handles: Vec::new(), _sa: None, mask: None, holds: std::sync::Mutex::default() })
        }

        /// Snapshot a mapped section's current bytes into an OWNED buffer.
        ///
        /// A Chroma section is interior-mutable shared memory: the connected game writes it
        /// cross-process, and our own `chroma-arbiter` thread writes the app-registry /
        /// `SessionInfo` pages during activation (see [`write_grant`]). So we must NEVER hand out
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

        /// The newest frame a game wrote for `class`, if any.
        #[must_use]
        pub fn device_frame(&self, class: DeviceClass) -> Option<DeviceFrame> {
            decode_device(&self.section_bytes(class.section())?, class)
        }

        /// The newest frame of every class a game has written.
        #[must_use]
        pub fn frames(&self) -> Vec<DeviceFrame> {
            DeviceClass::ALL.into_iter().filter_map(|c| self.device_frame(c)).collect()
        }

        /// A terse readout of what the connected game is painting: how many classes show a
        /// lit frame, and the effect (the keyboard's if it's lit, else the first lit class's).
        #[must_use]
        pub fn live_summary(&self) -> Option<(usize, &'static str)> {
            let lit: Vec<DeviceFrame> = self
                .frames()
                .into_iter()
                .filter(|f| f.cells.iter().any(|&(r, g, b)| (r | g | b) != 0) || f.cells.is_empty() && f.effect != Effect::None)
                .collect();
            let effect = lit
                .iter()
                .find(|f| f.class == DeviceClass::Keyboard)
                .or(lit.first())
                .map(|f| f.effect.name())?;
            Some((lit.len(), effect))
        }

        /// A device section's ring write head (`u32` at offset 0). The game bumps it once per
        /// committed frame, so a changed value is the cheap "new frame" signal: 4 bytes read
        /// instead of a whole-section snapshot. `None` if the section isn't mapped.
        #[must_use]
        pub fn frame_head(&self, device_type: u8) -> Option<u32> {
            let guid = device_section(device_type)?;
            let s = self.sections.iter().find(|s| s.guid == guid)?;
            if s.size < 4 {
                return None;
            }
            let mut b = [0u8; 4];
            // SAFETY: `s.view` is a live `s.size`-byte mapping for `self`'s lifetime and 4 <= size.
            // Volatile byte reads form no reference into the page (see `section_bytes`).
            unsafe {
                for (i, x) in b.iter_mut().enumerate() {
                    *x = std::ptr::read_volatile(s.view.add(i));
                }
            }
            Some(u32::from_le_bytes(b))
        }

        /// Paint `frame` (the class grid, row-major) for `device_type` in place of the game's own
        /// frames until cleared with `None`.
        pub fn set_hold(&self, device_type: u8, frame: Option<Vec<(u8, u8, u8)>>) {
            let mut holds = self.holds.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            holds.retain(|(dt, _)| *dt != device_type);
            if let Some(f) = frame {
                holds.push((device_type, f));
            }
        }

        /// The frame currently held for `device_type`, if any.
        #[must_use]
        pub fn hold(&self, device_type: u8) -> Option<Vec<(u8, u8, u8)>> {
            self.holds
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .iter()
                .find(|(dt, _)| *dt == device_type)
                .map(|(_, f)| f.clone())
        }

        /// The apps currently registered in the app registry (`D4E1A960`).
        #[must_use]
        pub fn registered_apps(&self) -> Vec<AppEntry> {
            self.section_bytes(APP_REGISTRY).map(|b| parse_app_registry(&b)).unwrap_or_default()
        }

        /// True while any Chroma game is actually running — a registered app whose PID
        /// is a live process. The app registry always carries a connected client's PID
        /// ([`AppEntry::id`]) even when the exe-NAME field is blank against our server,
        /// and the PID tracks the process exactly, so this is the honest "a game is
        /// connected" signal the fading layer gates on: a registry row that lingers
        /// after a game exits has a dead PID, so it never keeps the layer lit.
        #[must_use]
        pub fn any_client_connected(&self) -> bool {
            self.registered_apps().iter().any(|a| process_alive(a.id))
        }

        /// The most recent session-table entry, if any (`D41D8537`).
        #[must_use]
        pub fn latest_session(&self) -> Option<SessionTable> {
            self.section_bytes(SESSION_TABLE).and_then(|b| parse_session_table(&b))
        }

        /// True if a specific app (by exe name) holds its `Global\<exe>_rz`
        /// registration mutex — i.e. is currently connected.
        #[must_use]
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
        let ok = unsafe { GetExitCodeProcess(h, &raw mut code) };
        unsafe { CloseHandle(h) };
        ok != 0 && code == STILL_ACTIVE
    }

    // ──────────────────────── the arbitration arbiter ────────────────────────
    //
    // A shipping game connects to a bare server, but only STREAMS + paints real
    // colour when it believes the vendor arbitration layer is fully alive AND its session
    // has been GRANTED + ACTIVATED. Neuron supplies all of it, with zero vendor software,
    // on one thread — two responsibilities, both STANDING (nothing on a per-frame timer):
    //   • MASK   — hold the arbitration mutexes (incl. a per-user one) and keep the
    //     arbiter-ready events SIGNALLED (manual-reset, set once), so the game doesn't
    //     self-mute to near-black. These are HELD for the server's life, never re-poked.
    //   • GRANT + ONLINE (one-shot per client, see `activate_once`) — the client's access check
    //     is `server online && its PID == app registry +0x20C`. We write the client PID there
    //     and a SessionInfo slot {event 8 = WTS unlock, session-id} (see `write_grant`), wake
    //     the client's session worker ({B8B918C0}), then set the server-online event
    //     ({A84AF9C8}). The client re-evaluates, gains access, and replays its stored effects;
    //     from then on it streams frames and we only check its PID is alive.
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
    /// The client session worker's wake event — `SetEvent` to deliver the grant.
    const SESSION_WORKER_EVENT: &str = "{B8B918C0-9790-47F2-AC7A-F36B8414140C}";
    /// "Server online": the client sets its online flag and re-evaluates access on it. Until
    /// both online and the PID grant hold, every effect builder returns early, which is why a
    /// granted-but-not-online game streamed only a muted frame.
    const SERVER_ONLINE_EVENT: &str = "{A84AF9C8-EFE0-430D-871C-10DA760C2CCD}";
    /// The `SessionInfo` section the grant writes its {event-type, session-id} slots into.
    const SESSION_INFO: &str = "821AA2A2-8215-4A16-BE9D-7CD8CEBDC398";
    /// The Chroma client DLL a game loads — the marker we scan processes for.
    const CHROMA_CLIENT_DLL: &str = "rzchromasdk64.dll";

    /// A grant candidate must have completed client registration AND still carry the SDK
    /// module. Kept as a tiny pure predicate so fallback discovery's identity contract stays
    /// pinned independently of Win32 process enumeration.
    fn client_identity_matches(registered: bool, module_loaded: bool) -> bool {
        registered && module_loaded
    }

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
    impl Drop for SendHandle {
        fn drop(&mut self) {
            unsafe { CloseHandle(self.0) };
        }
    }

    /// The single in-process arbiter slot. `mask_worn()` above is a CROSS-process check
    /// (it opens a named kernel mutex) and has a TOCTOU window: two threads racing
    /// [`ShmServer::create`] can both observe "not worn" before either has actually created
    /// the mask mutexes, and both would then stand up their own `chroma-arbiter` thread —
    /// two threads writing the SAME app-registry/SessionInfo pages via `write_grant`. That is
    /// exactly the silent shared-memory race the `unsafe impl Sync for ShmServer` justification
    /// (see its comment below) assumes can't happen: "the one in-process writer is a single
    /// thread". This atomic makes that assumption true by construction, cheaply (no syscall)
    /// and deterministically, on top of the cross-process check.
    static MASK_CLAIMED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

    /// Claim the single in-process arbiter slot. `true` = claimed, the caller may stand up the
    /// arbiter thread; `false` = a [`MaskGuard`] already owns it in this process — the caller
    /// must stand down exactly as it would for [`CreateError::AlreadyServing`].
    fn claim_mask_slot() -> bool {
        MASK_CLAIMED
            .compare_exchange(
                false,
                true,
                std::sync::atomic::Ordering::AcqRel,
                std::sync::atomic::Ordering::Acquire,
            )
            .is_ok()
    }

    /// Release the in-process arbiter slot after the guard drops or mask setup fails.
    fn release_mask_slot() {
        MASK_CLAIMED.store(false, std::sync::atomic::Ordering::Release);
    }

    /// Bound on how long [`MaskGuard::drop`] will wait for `chroma-arbiter` to notice the
    /// stop flag and exit, via [`crate::worker::join_bounded`]. The loop's designed stop-check
    /// cadence is the 2s poll sleep in `arbiter_loop` — comfortably inside this deadline even
    /// counting one worst-case `find_chroma_client` process scan or the 60ms `activate_once`
    /// beat. Exceeding it means that design bound was violated (a scan wedged, not routine
    /// slowness) and `join_bounded` leaks the thread rather than hanging the caller — the same
    /// trade-off every other owned thread in this crate makes at Drop.
    const MASK_GUARD_DROP_DEADLINE: std::time::Duration = std::time::Duration::from_secs(3);

    /// Owns the arbitration objects + the background arbiter thread. Dropping it stops
    /// the thread (bounded join) THEN releases every held handle — so no tick can fire after
    /// the sections it writes have been torn down — and finally frees the in-process arbiter
    /// slot so a subsequent [`wear_mask`] can claim it.
    pub struct MaskGuard {
        held: Vec<SendHandle>,
        stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
        thread: Option<std::thread::JoinHandle<()>>,
    }
    impl Drop for MaskGuard {
        fn drop(&mut self) {
            self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
            if let Some(t) = self.thread.take() {
                crate::worker::join_bounded(t, MASK_GUARD_DROP_DEADLINE, "chroma-arbiter");
            }
            self.held.clear();
            release_mask_slot();
        }
    }

    /// Stand up the full arbitration: hold the mutexes + signalled events, then run the
    /// one-shot grant/activate loop on a background thread (NO pulse — see the arbiter
    /// block above). `appreg`/`sessinfo` are the mapped `(pointer, size)` of the
    /// app-registry and `SessionInfo` sections the grant writes.
    ///
    /// Refuses a second in-process arbiter and fails if a required mask object or thread cannot
    /// start. Partial masks must never be reported as native Chroma service.
    fn wear_mask(
        sa: &EveryoneSa,
        appreg: (usize, usize),
        sessinfo: (usize, usize),
        keyboard: (usize, usize),
    ) -> Result<MaskGuard, CreateError> {
        if !claim_mask_slot() {
            return Err(CreateError::AlreadyServing);
        }
        let mut held = Vec::new();
        // Arbitration mutexes: the three fixed + one per interactive user.
        let user = std::env::var("USERNAME").unwrap_or_default();
        let mut mutex_names: Vec<String> = MASK_MUTEXES.iter().map(|s| (*s).to_string()).collect();
        mutex_names.push(format!("{MASK_PERUSER_MUTEX}{user}"));
        for g in &mutex_names {
            let w = wide(&format!("Global\\{g}"));
            let h = create_named_mutex(sa.ptr(), &w);
            if h.is_null() {
                release_mask_slot();
                return Err(io::Error::last_os_error().into());
            }
            held.push(SendHandle(h));
        }
        // Events the game wants held SIGNALLED. Created manual-reset + START signalled and
        // kept alive in `held` for the server's lifetime. A manual-reset event stays set
        // until explicitly reset, so there is nothing to re-assert on a timer — re-poking
        // them would be exactly the periodic tick we want to avoid.
        for g in MASK_SIGNALLED_EVENTS {
            let w = wide(&format!("Global\\{g}"));
            let h = unsafe { CreateEventW(sa.ptr(), 1, 1, w.as_ptr()) }; // manual-reset, START signalled
            if h.is_null() {
                release_mask_slot();
                return Err(io::Error::last_os_error().into());
            }
            held.push(SendHandle(h));
        }
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let stop_thread = std::sync::Arc::clone(&stop);
        let thread = crate::worker::spawn_named("chroma-arbiter", move || {
            arbiter_loop(appreg, sessinfo, keyboard, stop_thread);
        });
        match thread {
            Ok(thread) => Ok(MaskGuard { held, stop, thread: Some(thread) }),
            Err(err) => {
                release_mask_slot();
                Err(err.into())
            }
        }
    }

    /// How often the quiet arbiter checks its state. Client-owned transport objects make
    /// discovery happen on the next tick; they are hints, not correctness requirements.
    const ARBITER_TICK: std::time::Duration = std::time::Duration::from_secs(2);

    /// Even when every client-owned event was transient (or a newer SDK changes which one
    /// survives), authoritative process/module discovery still runs at this bounded cadence.
    /// This keeps true idle cheap without letting a missed hint strand a live game forever.
    const IDLE_CLIENT_RESCAN_INTERVAL: std::time::Duration = std::time::Duration::from_secs(15);

    /// Scheduling state for Chroma-client discovery. A newly-created server starts armed, so
    /// enabling/re-enabling Chroma is an immediate rescan boundary rather than waiting for the
    /// game to recreate a one-shot event. Kernel objects only accelerate subsequent scans.
    #[derive(Clone, Copy, Debug)]
    struct ClientDiscovery {
        next_fallback_scan: std::time::Instant,
    }

    impl ClientDiscovery {
        fn armed(now: std::time::Instant) -> Self {
            Self { next_fallback_scan: now }
        }

        fn scan_due(&self, now: std::time::Instant, transport_hint: bool) -> bool {
            transport_hint || now >= self.next_fallback_scan
        }

        fn note_scan(&mut self, now: std::time::Instant) {
            self.next_fallback_scan = now + IDLE_CLIENT_RESCAN_INTERVAL;
        }

        fn rearm(&mut self, now: std::time::Instant) {
            self.next_fallback_scan = now;
        }
    }

    /// The arbiter: ACTIVATE the connected game ONCE — write its grant, then set the
    /// server-online event — and otherwise stay QUIET. It is deliberately NOT a
    /// heartbeat. Once a game is activated it STAYS activated (its own frames flow), so the
    /// only ongoing work is a cheap liveness check on the known PID; we never re-poke the
    /// session. (An earlier "re-activate if the board looks uniform" recheck was removed: it
    /// occasionally caught the game's own transient clear frame and re-fired the activation
    /// mid-stream, which the game rendered as a periodic flicker.) The expensive
    /// process/module scan runs ONLY while no game is activated yet. Stable client-owned
    /// objects accelerate it, but no single event gates correctness: the first scan is
    /// immediate and a bounded idle fallback covers missed/transient SDK events. Re-activation
    /// happens only on a relaunch (a new PID appears after the old one dies).
    fn arbiter_loop(
        appreg: (usize, usize),
        sessinfo: (usize, usize),
        _keyboard: (usize, usize),
        stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    ) {
        use std::sync::atomic::Ordering;
        let mut activated: Option<u32> = None; // the PID we've activated
        let mut discovery = ClientDiscovery::armed(std::time::Instant::now());
        while !stop.load(Ordering::Relaxed) {
            if let Some(pid) = activated {
                // Activated: the only ongoing work is a cheap liveness check. A dead PID
                // (game closed) → drop it so a relaunch is picked up. Nothing is re-poked.
                if !process_alive(pid) {
                    activated = None;
                    discovery.rearm(std::time::Instant::now());
                }
            } else {
                // Client-owned objects are cheap positive hints; the bounded fallback catches
                // transient hints and newer SDKs that use different objects.
                let now = std::time::Instant::now();
                if discovery.scan_due(now, client_transport_present()) {
                    discovery.note_scan(now);
                    if let Some(pid) = find_chroma_client() {
                        unsafe { activate_once(appreg, sessinfo, pid, pid_session(pid)) };
                        activated = Some(pid);
                    }
                }
            }
            std::thread::sleep(ARBITER_TICK);
        }
    }

    /// The decompile-exact ACTIVATION (mirrors the reference `activate2` sequence): register
    /// the client in the app registry, seed one session slot, wake the session worker, then
    /// — after a short beat — set [`SERVER_ONLINE_EVENT`]. With the PID grant in place that
    /// completes the client's access check, and it replays its effects.
    ///
    /// # Safety
    /// `appreg`/`sessinfo` must be the live mapped views of the app-registry / `SessionInfo`
    /// sections with the given sizes; only this thread writes them.
    unsafe fn activate_once(appreg: (usize, usize), sessinfo: (usize, usize), pid: u32, sess: u32) {
        write_grant(appreg, sessinfo, pid, sess);
        signal_event(SESSION_WORKER_EVENT);
        std::thread::sleep(std::time::Duration::from_millis(60));
        signal_event(SERVER_ONLINE_EVENT);
    }

    /// The MEMORY half of activation: stamp the grant records into the mapped app-registry and
    /// `SessionInfo` pages. Split out from [`activate_once`] so the exact byte layout is unit-tested
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
            std::ptr::write_unaligned(p.cast::<u32>(), 1); // app count = 1
            std::ptr::write_unaligned(p.add(APP_REGISTRY_RECORD0 + 0x0c).cast::<u32>(), pid); // PID @ +0x20c
        }
        let (sp, sp_size) = sessinfo;
        if sp != 0 && sp_size >= 12 {
            let s = sp as *mut u8;
            std::ptr::write_unaligned(s.cast::<u32>(), 0); // head
            std::ptr::write_unaligned(s.add(4).cast::<u32>(), 8); // slot0 event-type 8 (grant access)
            std::ptr::write_unaligned(s.add(8).cast::<u32>(), sess); // slot0 session id
        }
    }

    #[cfg(test)]
    mod discovery_tests {
        use super::{ClientDiscovery, IDLE_CLIENT_RESCAN_INTERVAL};
        use std::time::{Duration, Instant};

        #[test]
        fn a_new_or_reenabled_server_scans_immediately_without_an_event() {
            let now = Instant::now();
            let discovery = ClientDiscovery::armed(now);

            assert!(
                discovery.scan_due(now, false),
                "creating the Chroma face is an immediate rescan boundary"
            );
        }

        #[test]
        fn a_client_object_accelerates_discovery_but_is_not_required() {
            let now = Instant::now();
            let mut discovery = ClientDiscovery::armed(now);
            discovery.note_scan(now);
            let before_fallback = now + Duration::from_secs(1);

            assert!(!discovery.scan_due(before_fallback, false), "true idle stays cheap");
            assert!(
                discovery.scan_due(before_fallback, true),
                "any durable client transport witness triggers the next scan"
            );
            assert!(
                discovery.scan_due(now + IDLE_CLIENT_RESCAN_INTERVAL, false),
                "a missed/transient event cannot gate discovery forever"
            );
        }

        #[test]
        fn a_dead_activated_client_rearms_discovery() {
            let now = Instant::now();
            let mut discovery = ClientDiscovery::armed(now);
            discovery.note_scan(now);
            let relaunched = now + Duration::from_secs(1);

            discovery.rearm(relaunched);
            assert!(
                discovery.scan_due(relaunched, false),
                "process death must make the replacement PID discoverable immediately"
            );
        }

        #[test]
        fn fallback_candidates_must_be_registered_and_have_the_sdk_loaded() {
            use super::client_identity_matches;

            assert!(client_identity_matches(true, true));
            assert!(!client_identity_matches(true, false), "a stale registration is not a client");
            assert!(!client_identity_matches(false, true), "a passive DLL load is not a client");
            assert!(!client_identity_matches(false, false));
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

    #[cfg(test)]
    mod mask_slot_tests {
        use super::{claim_mask_slot, release_mask_slot};

        // Pure atomic-only guard test — no kernel objects, no live `wear_mask`/`MaskGuard`.
        // Serialized with a process-wide lock because `MASK_CLAIMED` is a single global static
        // and `cargo test` runs tests in this module concurrently by default; without this the
        // two tests below would race each other's claim/release calls.
        static TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

        #[test]
        fn second_concurrent_claim_is_refused() {
            let _guard = TEST_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            assert!(claim_mask_slot(), "first claim succeeds (slot starts free)");
            assert!(
                !claim_mask_slot(),
                "a second concurrent claim (simulating a second in-process MaskGuard) must be refused"
            );
            release_mask_slot();
        }

        #[test]
        fn claim_is_reusable_after_release() {
            let _guard = TEST_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            assert!(claim_mask_slot(), "slot free at test start (serialized by TEST_LOCK)");
            release_mask_slot();
            assert!(claim_mask_slot(), "claim succeeds again once the prior guard released it");
            assert!(!claim_mask_slot(), "still refuses a second claim while the first is held");
            release_mask_slot();
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

    /// Open a named event only long enough to prove it exists. `SYNCHRONIZE` is sufficient
    /// for a presence probe and is deliberately less demanding than full control: client
    /// objects use the game's token DACL, not the Everyone DACL on server-owned objects.
    fn named_event_present(name: &str) -> bool {
        const SYNCHRONIZE: u32 = 0x0010_0000;
        let w = wide(name);
        let h = unsafe { OpenEventW(SYNCHRONIZE, 0, w.as_ptr()) };
        if h.is_null() {
            false
        } else {
            unsafe { CloseHandle(h) };
            true
        }
    }

    /// Mutex counterpart to [`named_event_present`]. Opening never waits for or acquires the
    /// mutex; it is a side-effect-free liveness hint.
    fn named_mutex_present(name: &str) -> bool {
        const SYNCHRONIZE: u32 = 0x0010_0000;
        let w = wide(name);
        let h = unsafe { OpenMutexW(SYNCHRONIZE, 0, w.as_ptr()) };
        if h.is_null() {
            false
        } else {
            unsafe { CloseHandle(h) };
            true
        }
    }

    /// True when the live client side has left ANY known transport witness. Older code used
    /// only `SESSION_WORKER_EVENT`; that event is on-demand/transient, so enabling Neuron after
    /// a game had initialized could miss it forever even though the durable activation events
    /// and registration mutex were still present. The canonical object map now supplies all
    /// stable client-created hints. This only accelerates discovery — [`ClientDiscovery`]'s
    /// immediate/fallback scans remain authoritative when no known witness survives.
    fn client_transport_present() -> bool {
        let session_worker = format!("Global\\{SESSION_WORKER_EVENT}");
        named_event_present(&session_worker)
            || client_objects().any(|o| match o.kind {
                Kind::Event => named_event_present(&o.name()),
                Kind::Mutex => named_mutex_present(&o.name()),
                Kind::Section(_) | Kind::Unknown => false,
            })
    }

    /// The interactive session id for a pid (games run in the console session, usually 1).
    fn pid_session(pid: u32) -> u32 {
        let mut sess: u32 = 0;
        if unsafe { ProcessIdToSessionId(pid, &raw mut sess) } != 0 {
            sess
        } else {
            1
        }
    }

    /// Find a live, REGISTERED process with the Chroma client DLL loaded — the game to
    /// grant. The per-exe `Global\<stem>_rz` mutex is the durable client-side registration
    /// witness; requiring it prevents an idle process that merely loaded the SDK DLL from
    /// winning an authoritative fallback scan. Checking the cheap mutex first also avoids
    /// taking a module snapshot for every unrelated process on the machine.
    ///
    /// Returns the first match (the common case is a single game); skips processes we can't
    /// snapshot (bitness / access), which is harmless.
    fn find_chroma_client() -> Option<u32> {
        let snap = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
        if snap == INVALID_HANDLE_VALUE {
            return None;
        }
        let mut pe: PROCESSENTRY32W = unsafe { std::mem::zeroed() };
        pe.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
        let mut found = None;
        if unsafe { Process32FirstW(snap, &raw mut pe) } != 0 {
            loop {
                let pid = pe.th32ProcessID;
                if pid > 4 {
                    let end =
                        pe.szExeFile.iter().position(|&c| c == 0).unwrap_or(pe.szExeFile.len());
                    let exe = String::from_utf16_lossy(&pe.szExeFile[..end]);
                    let registered = named_mutex_present(&rz_mutex_name(&exe));
                    let module_loaded = registered && process_has_module(pid, CHROMA_CLIENT_DLL);
                    if client_identity_matches(registered, module_loaded) {
                        found = Some(pid);
                        break;
                    }
                }
                if unsafe { Process32NextW(snap, &raw mut pe) } == 0 {
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
        if unsafe { Module32FirstW(snap, &raw mut me) } != 0 {
            loop {
                let end = me.szModule.iter().position(|&c| c == 0).unwrap_or(me.szModule.len());
                let name = String::from_utf16_lossy(&me.szModule[..end]).to_lowercase();
                if name == dll_lower {
                    hit = true;
                    break;
                }
                if unsafe { Module32NextW(snap, &raw mut me) } == 0 {
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
    /// the lower layers. `render` returns the decoded frame fitted to the surface's LEDs
    /// (see [`fit_cells`]).
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
        last_ts: u64,
        last_ts_change: Option<Instant>,
        /// The clock preset effects (Wave, Breathing, …) animate on.
        born: Instant,
        /// Shared game-lighting policy: blend, strength, fade, and device scope.
        policy: Arc<PaintPolicy>,
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
                born: Instant::now(),
                policy,
            }
        }
    }

    impl LiveContent for ChromaShmLayer {
        fn render(&mut self, now: Instant) -> Vec<Option<Rgb>> {
            // The newest frame for this class: pixels for Custom/Static, rendered for presets.
            let mut has_frame = false;
            let class = DeviceClass::from_bit(self.device_type);
            if let Some(frame) = class.and_then(|c| self.server.device_frame(c)) {
                if frame.effect != Effect::None {
                    let t = u64::try_from(now.duration_since(self.born).as_millis()).unwrap_or(0);
                    let source = self.server.hold(self.device_type).unwrap_or_else(|| frame.cells_at(t));
                    let mut cells: Vec<Option<Rgb>> = fit_cells(frame.class, &source, self.leds)
                        .into_iter()
                        .map(|(r, g, b)| Some(Rgb(r, g, b)))
                        .collect();
                    self.policy.lens().apply_cells(&mut cells);
                    self.last = cells;
                    has_frame = true;
                }
                // A game actively painting advances the write tick.
                if frame.tick_ms != self.last_ts {
                    self.last_ts = frame.tick_ms;
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
                born: self.born,
                policy: Arc::clone(&self.policy),
            })
        }
    }

    impl Drop for ShmServer {
        fn drop(&mut self) {
            // Stop the arbiter thread FIRST — it writes into the sections we unmap below.
            drop(self.mask.take());
            self.sections.clear();
            self.handles.clear();
        }
    }
    #[cfg(test)]
    mod tests {
        use super::*;
        use super::super::{CONTROL_SECTION, parse_roster};
        use windows_sys::Win32::Foundation::WAIT_OBJECT_0;
        use windows_sys::Win32::System::Threading::{ReleaseMutex, WaitForSingleObject};

        fn seed_test_name() -> String {
            let stamp = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("system clock")
                .as_nanos();
            format!(r"Local\neuron-chroma-seed-{}-{stamp}", std::process::id())
        }

        #[test]
        fn section_seed_keeps_a_mapping_open_until_drop() {
            let name = seed_test_name();
            let seed = SectionSeed::create_named([(name.clone(), 4096)]).expect("seed local mapping");
            assert_eq!(seed.section_count(), 1);
            let wide_name = wide(&name);
            let opened = unsafe { OpenFileMappingW(FILE_MAP_ALL_ACCESS, 0, wide_name.as_ptr()) };
            assert!(!opened.is_null(), "limited client can open the seeded mapping");
            unsafe { CloseHandle(opened) };
            drop(seed);
            let after = unsafe { OpenFileMappingW(FILE_MAP_ALL_ACCESS, 0, wide_name.as_ptr()) };
            assert!(after.is_null(), "the final handle closes when the seed drops");
        }

        /// A local named mapping exercises the broker-to-client contract without claiming any
        /// Global Chroma names. The captured device page is copied through the same volatile
        /// snapshot path used by `ShmServer`, then decoded by the production parser.
        ///
        /// `cargo test -p neuron-host --features bridge synthetic_mapping_snapshot_decodes_frame -- --nocapture`
        #[test]
        fn synthetic_mapping_snapshot_decodes_frame() {
            let name = seed_test_name();
            let bytes = include_bytes!("chroma_shm_data/overwatch-live-keyboard.bin");
            let seed = SectionSeed::create_named([(name.clone(), bytes.len())])
                .expect("create synthetic broker mapping");
            let wide_name = wide(&name);
            let client = unsafe { OpenFileMappingW(FILE_MAP_ALL_ACCESS, 0, wide_name.as_ptr()) };
            assert!(!client.is_null(), "synthetic client opens broker mapping");
            let view = unsafe { MapViewOfFile(client, FILE_MAP_ALL_ACCESS, 0, 0, bytes.len()) };
            assert!(!view.Value.is_null(), "synthetic client maps broker section");
            // SAFETY: the view is writable and exactly `bytes.len()` bytes, matching the mapped
            // section. This simulates one completed game frame in shared memory.
            unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), view.Value.cast::<u8>(), bytes.len()) };

            let reader_mapping = unsafe { OpenFileMappingW(FILE_MAP_WRITE, 0, wide_name.as_ptr()) };
            assert!(!reader_mapping.is_null(), "read-alongside client opens mapping");
            let reader_view = unsafe { MapViewOfFile(reader_mapping, FILE_MAP_WRITE, 0, 0, bytes.len()) };
            assert!(!reader_view.Value.is_null(), "read-alongside client maps section");
            let server = ShmServer {
                sections: vec![MappedSection {
                    guid: super::super::device_section(0x01).expect("keyboard section"),
                    mapping: reader_mapping,
                    view: reader_view.Value.cast::<u8>(),
                    size: bytes.len(),
                }],
                handles: Vec::new(),
                _sa: None,
                mask: None,
                holds: std::sync::Mutex::default(),
            };
            let frame = server.device_frame(DeviceClass::Keyboard).expect("keyboard frame through the mapping");
            assert_eq!(frame, decode_device(bytes, DeviceClass::Keyboard).expect("fixture decodes"));
            assert_eq!(frame.effect, Effect::Custom);
            assert_eq!(server.frames().len(), 1, "only the keyboard section is mapped");

            // The layer paints a hold instead of the game's frame, and paints everything
            // through the policy's lens.
            use crate::arbiter::{LiveContent, Rgb};
            use crate::paint::{Lens, PaintPolicy};
            let server = Arc::new(server);
            let policy = PaintPolicy::opaque();
            let mut layer = ChromaShmLayer::new(Arc::clone(&server), "kb".into(), 0x01, 4, Arc::clone(&policy), 1.0);
            server.set_hold(0x01, Some(vec![(10, 20, 30); 4]));
            let now = std::time::Instant::now();
            assert_eq!(layer.render(now), vec![Some(Rgb(10, 20, 30)); 4], "a hold replaces the game's frame");
            server.set_hold(0x01, None);
            policy.set_lens(Lens { hue_shift: 180, ..Lens::default() });
            let lens = policy.lens();
            let want: Vec<Option<Rgb>> = frame
                .cells
                .iter()
                .take(4)
                .map(|&(r, g, b)| Some(lens.apply(Rgb(r, g, b))))
                .collect();
            assert_eq!(layer.render(now), want, "the game's frame, through the lens");
            drop(layer);
            let server = Arc::try_unwrap(server).unwrap_or_else(|_| panic!("the layer released the server"));

            drop(server);
            unsafe {
                UnmapViewOfFile(view);
                CloseHandle(client);
            }
            drop(seed);
        }

        #[test]
        fn section_seed_rejects_an_existing_undersized_mapping() {
            let name = seed_test_name();
            let wide_name = wide(&name);
            let small = unsafe {
                CreateFileMappingW(
                    INVALID_HANDLE_VALUE, std::ptr::null(), PAGE_READWRITE,
                    0, 4096, wide_name.as_ptr(),
                )
            };
            assert!(!small.is_null(), "create undersized local mapping");
            let result = SectionSeed::create_named([(name, 8192)]);
            assert!(result.is_err(), "a stale small section cannot pass readiness");
            unsafe { CloseHandle(small) };
        }

        /// A broker install check. Opens every fixed section from this test process at Limited
        /// privilege and creates no objects or device output.
        #[test]
        #[ignore = "needs the protected Chroma broker running on this machine"]
        fn chroma_broker_seeded_sections_present() {
            assert!(seeded_sections_present(), "a required native Chroma section is absent or inaccessible");
        }

        /// Confirms a live tray owns the native server mask without creating or writing it.
        #[test]
        #[ignore = "needs the regular tray serving native Chroma on this machine"]
        fn chroma_live_mask_worn() {
            assert!(mask_worn(), "the regular tray is not serving native Chroma");
        }

        /// Read-only snapshot of whatever Chroma server is live right now: who is registered,
        /// which session is active, what the roster says, and whether any device buffer is
        /// receiving frames. Attaches with `ShmServer::open`, which maps existing sections and
        /// creates nothing, so it is safe to run against a server serving a live game.
        ///
        /// `cargo test -p neuron-host --features bridge chroma_peek -- --ignored --nocapture`
        #[test]
        #[ignore = "needs a live Chroma server (neuron's or the vendor's) on this machine"]
        fn chroma_peek() {
            let srv = match ShmServer::open() {
                Ok(s) => s,
                Err(e) => {
                    println!("no Chroma server to attach to: {e}");
                    return;
                }
            };

            match srv.section_bytes(SESSION_TABLE).as_deref().and_then(parse_session_table) {
                Some(t) => println!(
                    "session table: active_count={} session_id={:#x} session_handle={:#010x}",
                    t.active_count, t.session_id, t.session_handle
                ),
                None => println!("session table: no active session (nobody painting)"),
            }

            match srv.section_bytes(APP_REGISTRY) {
                Some(b) => {
                    let apps = parse_app_registry(&b);
                    println!("app registry: {} entry(s)", apps.len());
                    for a in &apps {
                        println!("   id={:#x} {}", a.id, a.name);
                    }
                }
                None => println!("app registry: section absent"),
            }

            match srv.section_bytes(CONTROL_SECTION).as_deref().and_then(parse_roster) {
                Some((n, first)) => println!("roster: {n} device(s), first = {first:?}"),
                None => println!("roster: unreadable"),
            }

            for class in DeviceClass::ALL {
                match srv.device_frame(class) {
                    Some(f) => {
                        let lit = f.cells.iter().filter(|c| (c.0 | c.1 | c.2) != 0).count();
                        println!(
                            "{}: {} (code {}), device {:#06x}, tick {}, {lit} lit of {}",
                            class.name(), f.effect.name(), f.effect_code, f.device, f.tick_ms, f.len()
                        );
                    }
                    None => println!("{}: idle", class.name()),
                }
            }
        }

        /// A client that waits on one of the server's mutexes must not block on us. Proven by
        /// waiting from another thread with a zero timeout: on an owned mutex that returns
        /// WAIT_TIMEOUT (ownership is per-thread), on an unowned one it acquires immediately.
        ///
        /// Uses the `Local\` namespace so the test needs no elevation; `Global\` differs only in
        /// visibility across sessions, not in ownership semantics.
        #[test]
        fn a_server_mutex_never_blocks_a_waiting_client() {
            let name = wide(&format!(r"Local\neuron-shm-mutex-test-{}", std::process::id()));
            let h = create_named_mutex(std::ptr::null(), &name);
            assert!(!h.is_null(), "CreateMutexW failed");

            let addr = h as usize;
            let waited = std::thread::spawn(move || {
                let h = addr as HANDLE;
                let r = unsafe { WaitForSingleObject(h, 0) };
                if r == WAIT_OBJECT_0 {
                    unsafe { ReleaseMutex(h) };
                }
                r
            })
            .join()
            .expect("join waiter");

            unsafe { CloseHandle(h) };
            assert_eq!(
                waited, WAIT_OBJECT_0,
                "another thread must be able to take the mutex; a non-zero result means we created \
                 it owned, which blocks a waiting Chroma client indefinitely"
            );
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

    // ── frame codec, pinned to REAL Overwatch sections ──
    // `overwatch-live-*` are the newest ring of each class Overwatch wrote during a match
    // (2026-09-28); `overwatch-*-section` is an older capture taken while the game had
    // suspended its Chroma output.

    const LIVE_KEYBOARD: &[u8] = include_bytes!("chroma_shm_data/overwatch-live-keyboard.bin");
    const LIVE_MOUSE: &[u8] = include_bytes!("chroma_shm_data/overwatch-live-mouse.bin");
    const LIVE_HEADSET: &[u8] = include_bytes!("chroma_shm_data/overwatch-live-headset.bin");
    const LIVE_MOUSEPAD: &[u8] = include_bytes!("chroma_shm_data/overwatch-live-mousepad.bin");
    const LIVE_LINK: &[u8] = include_bytes!("chroma_shm_data/overwatch-live-chroma-link.bin");
    const SUSPENDED_KEYBOARD: &[u8] = include_bytes!("chroma_shm_data/overwatch-keyboard-section.bin");
    const SUSPENDED_MOUSE: &[u8] = include_bytes!("chroma_shm_data/overwatch-device02-section.bin");

    /// The match's resting colour: what Overwatch put on every non-keyboard class.
    const MATCH_AMBIENT: Rgb = (121, 97, 78);

    #[test]
    fn a_live_keyboard_frame_is_the_custom_grid() {
        let f = decode_device(LIVE_KEYBOARD, DeviceClass::Keyboard).expect("keyboard frame");
        assert_eq!(f.effect, Effect::Custom);
        assert_eq!(f.device, 0xFFFF, "addressed to every keyboard");
        assert_eq!((f.rows, f.cols, f.cells.len()), (6, 22, 132));
        let at = |r: usize, c: usize| f.cells[r * 22 + c];
        assert_eq!(at(2, 3), (222, 153, 0), "W wears the movement colour");
        assert_eq!([at(3, 2), at(3, 3), at(3, 4)], [(222, 153, 0); 3], "A S D too");
        assert_eq!(at(0, 0), (30, 24, 19), "the background");
    }

    #[test]
    fn every_other_class_decodes_to_the_matchs_static_colour() {
        for (bytes, class, device) in [
            (LIVE_MOUSE, DeviceClass::Mouse, 0xFFFF),
            (LIVE_HEADSET, DeviceClass::Headset, 0x0F19),
            (LIVE_MOUSEPAD, DeviceClass::Mousepad, 0xFFFF),
        ] {
            let f = decode_device(bytes, class).unwrap_or_else(|| panic!("{class:?} frame"));
            assert_eq!(f.effect, Effect::Static, "{class:?} code {} is Static", f.effect_code);
            assert_eq!(f.device, device, "{class:?}");
            assert_eq!(f.uniform(), Some(MATCH_AMBIENT), "{class:?}");
        }
        let (_, rows, cols) = MOUSE.grid;
        assert_eq!((rows, cols), (9, 7));
    }

    #[test]
    fn chroma_link_led_zero_carries_the_ambient() {
        let f = decode_device(LIVE_LINK, DeviceClass::ChromaLink).expect("link frame");
        assert_eq!(f.effect, Effect::Custom);
        assert_eq!(f.cells.len(), 5, "the API's 5 LEDs, not the 50 slots the client copies");
        assert_eq!(f.cells[0], MATCH_AMBIENT, "CL1 is the base colour every partner LED takes");
    }

    #[test]
    fn a_suspended_game_is_read_as_suspended_not_as_colour() {
        for (bytes, class) in [(SUSPENDED_KEYBOARD, DeviceClass::Keyboard), (SUSPENDED_MOUSE, DeviceClass::Mouse)] {
            let f = decode_device(bytes, class).expect("record present");
            assert_eq!(f.effect, Effect::Other(13));
            assert_eq!(effect_name(f.effect_code), "Suspend");
            assert!(f.cells.is_empty());
        }
    }

    #[test]
    fn a_section_of_another_class_or_an_empty_one_is_rejected() {
        assert!(decode_device(LIVE_KEYBOARD, DeviceClass::Mouse).is_none(), "wrong class");
        assert!(decode_device(&vec![0u8; 40_000], DeviceClass::Keyboard).is_none(), "never written");
        assert!(decode_device(&LIVE_HEADSET[..100], DeviceClass::Headset).is_none(), "shorter than the ring");
    }

    #[test]
    fn the_effect_enum_names_the_verified_codes() {
        assert_eq!(effect_name(6), "Static", "Overwatch's mouse/pad/headset code");
        assert_eq!(effect_name(7), "Custom");
        assert_eq!(effect_name(3), "Breathing");
        assert_eq!(effect_name(1), "Wave");
        assert_eq!(effect_name(0x2A), "Unknown");
    }

    /// A one-class section whose newest record carries `effect` and the given plaintext
    /// parameter words, for exercising the preset effects no capture has shown.
    fn synth(class: DeviceClass, effect: u32, words: &[(usize, u32)]) -> Vec<u8> {
        let l = class.layout();
        let mut b = vec![0u8; RECORD0 + RING_DEPTH * l.stride];
        b[0..4].copy_from_slice(&1u32.to_le_bytes()); // head 1 → newest is slot 0
        let rec = RECORD0;
        b[rec..rec + 4].copy_from_slice(&((u32::from(class.bit()) << 16) | 0xFFFF).to_le_bytes());
        b[rec + l.effect..rec + l.effect + 4].copy_from_slice(&effect.to_le_bytes());
        for &(off, v) in words {
            b[rec + off..rec + off + 4].copy_from_slice(&v.to_le_bytes());
        }
        b
    }

    #[test]
    fn preset_effects_arrive_as_parameters_and_are_rendered() {
        let two = synth(DeviceClass::Keyboard, 3, &[(0x18, 2), (0x1C, 0x0000_00FF), (0x20, 0x00FF_0000)]);
        let f = decode_device(&two, DeviceClass::Keyboard).expect("breathing");
        assert_eq!(f.effect, Effect::Breathing { colours: Some(((255, 0, 0), (0, 0, 255))) });
        assert!(f.cells.is_empty(), "no pixels in the record");
        assert_eq!(f.cells_at(0), vec![(0, 0, 0); 132], "a breath starts dark");
        assert_eq!(f.cells_at(2000)[0], (255, 0, 0), "peaks at the first colour");
        assert_eq!(f.cells_at(6000)[0], (0, 0, 255), "the next breath takes the second");

        let wave = decode_device(&synth(DeviceClass::Keyboard, 1, &[(0x0C, 2)]), DeviceClass::Keyboard).expect("wave");
        assert_eq!(wave.effect, Effect::Wave { reverse: true });
        let frame = wave.cells_at(0);
        assert_ne!(frame[0], frame[11], "a wave varies across columns");
        assert_eq!(frame[0], frame[22], "and not down a column");

        let spectrum = decode_device(&synth(DeviceClass::Mousepad, 2, &[]), DeviceClass::Mousepad).expect("spectrum");
        assert_eq!(spectrum.effect, Effect::Spectrum);
        assert_ne!(spectrum.cells_at(0)[0], spectrum.cells_at(3000)[0], "it cycles");
    }

    #[test]
    fn custom_key_overrides_win_over_the_colour_plane() {
        let l = DeviceClass::Keyboard.layout();
        let (colour, key) = l.key_planes.expect("keyboard has key planes");
        // tick 0 → phase 0: XOR every colour byte with the keystream so it decodes to the value.
        let enc = |rgb: [u8; 4]| -> u32 {
            let k = |b: usize| KEYSTREAM[b * KEYSTREAM_CHANNEL_STRIDE];
            u32::from_le_bytes([rgb[0] ^ k(0), rgb[1] ^ k(1), rgb[2] ^ k(2), rgb[3] ^ k(3)])
        };
        let b = synth(
            DeviceClass::Keyboard,
            8,
            &[(colour, enc([10, 20, 30, 0])), (colour + 4, enc([10, 20, 30, 0])), (key + 4, enc([200, 0, 0, 1]))],
        );
        let f = decode_device(&b, DeviceClass::Keyboard).expect("custom key");
        assert_eq!(f.effect, Effect::CustomKey);
        assert_eq!(f.cells[0], (10, 20, 30), "no override: the colour plane");
        assert_eq!(f.cells[1], (200, 0, 0), "flagged key: the override");
    }

    #[test]
    fn fitting_a_class_frame_onto_physical_leds() {
        let grid: Vec<Rgb> = (0..132).map(|i| (i as u8, 0, 0)).collect();
        assert_eq!(fit_cells(DeviceClass::Keyboard, &grid, 4), vec![(0, 0, 0), (1, 0, 0), (2, 0, 0), (3, 0, 0)]);
        assert_eq!(fit_cells(DeviceClass::Headset, &[(9, 9, 9); 5], 5), vec![(9, 9, 9); 5], "same count maps straight");
        assert_eq!(fit_cells(DeviceClass::Mouse, &[MATCH_AMBIENT; 63], 3), vec![MATCH_AMBIENT; 3], "a solid frame fills");
        let mut mixed = vec![(0, 0, 0); 63];
        mixed[0] = (100, 0, 0);
        mixed[1] = (0, 100, 0);
        assert_eq!(fit_cells(DeviceClass::Mouse, &mixed, 2), vec![(50, 50, 0); 2], "else the lit cells' average");
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
        // The low dword of the registration tick (`f6 b3 b7 0c` in memory).
        assert_eq!(s.session_handle, 0x0c_b7_b3_f6);
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
    fn every_class_section_and_frame_event_is_in_the_object_map() {
        for class in DeviceClass::ALL {
            assert_eq!(device_section(class.bit()), Some(class.section()));
            let sec = OBJECTS.iter().find(|o| o.guid == class.section()).expect("section in map");
            assert!(matches!(sec.kind, Kind::Section(n) if n >= RECORD0 + RING_DEPTH * class.layout().stride), "{class:?}");
            assert!(OBJECTS.iter().any(|o| o.guid == class.frame_event()), "{class:?} frame event in map");
        }
        assert_eq!(device_section(0x00), None);
    }

    #[test]
    fn session_table_and_registry_guids_are_consistent() {
        assert!(OBJECTS.iter().any(|o| o.guid == SESSION_TABLE && matches!(o.kind, Kind::Section(168))));
        assert!(OBJECTS.iter().any(|o| o.guid == APP_REGISTRY && matches!(o.kind, Kind::Section(26932))));
        let o = OBJECTS.iter().find(|o| o.guid == SESSION_TABLE_EVENT).expect("session-table event in map");
        assert_eq!(o.kind, Kind::Event);
    }

    // ── KEYSTREAM index safety ──
    //
    // Only the low 7 bits of a (possibly hostile) tick reach the keystream index, clamped by
    // `frame_phase`; the channel multiplier is a literal. Exhaust the domain and pin the mask.

    #[test]
    fn keystream_indices_stay_in_bounds_for_every_phase() {
        for low7 in 0u64..128 {
            let phase = frame_phase(low7);
            assert!(phase + 3 * KEYSTREAM_CHANNEL_STRIDE < KEYSTREAM.len(), "low7={low7} phase={phase}");
        }
        for tick in [0, 0x7f, 0x80, 0xFFFF_FFFF, 0x8000_0000_0000_0000, u64::MAX, u64::MAX - 1] {
            assert!(frame_phase(tick) <= 124, "tick={tick:#x}");
        }
    }

    #[test]
    fn every_tick_decodes_every_class_without_panicking() {
        for class in DeviceClass::ALL {
            for low7 in 0u32..128 {
                let l = class.layout();
                let mut b = vec![0xAAu8; RECORD0 + RING_DEPTH * l.stride];
                b[0..4].copy_from_slice(&1u32.to_le_bytes());
                b[RECORD0..RECORD0 + 4].copy_from_slice(&((u32::from(class.bit()) << 16) | 0xFFFF).to_le_bytes());
                for effect in [0u32, 1, 2, 3, 4, 5, 6, 7, 8, 13] {
                    b[RECORD0 + l.effect..RECORD0 + l.effect + 4].copy_from_slice(&effect.to_le_bytes());
                    b[RECORD0 + l.tick..RECORD0 + l.tick + 8].copy_from_slice(&u64::from(low7).to_le_bytes());
                    if let Some(f) = decode_device(&b, class) {
                        let _ = f.cells_at(u64::from(low7) * 37);
                    }
                }
            }
        }
    }

    // ── torn-read tolerance (the invariant behind `unsafe impl Sync for ShmServer`) ──
    //
    // `section_bytes` copies memory the game is writing, so a snapshot can be torn. The
    // decoder must reject or decode such bytes without panicking, and its output is always
    // exactly one class grid: offsets are fixed and every read is bounds-checked.

    /// A seeded PRNG (SplitMix64): deterministic across runs, no `rand` dep.
    struct Lcg(u64);
    impl Lcg {
        fn next_u64(&mut self) -> u64 {
            self.0 = self.0.wrapping_add(0x9E3779B97F4A7C15);
            let mut z = self.0;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D049BB133111EB);
            z ^ (z >> 31)
        }
        fn next_below(&mut self, bound: usize) -> usize {
            (self.next_u64() % bound as u64) as usize
        }
    }

    fn assert_bounded(section: &[u8], class: DeviceClass) {
        if let Some(f) = decode_device(section, class) {
            assert!(f.cells.is_empty() || f.cells.len() == f.len(), "{class:?} decoded {} cells", f.cells.len());
            assert_eq!(f.cells_at(1234).len(), f.len());
        }
    }

    #[test]
    fn splices_and_zeroed_tails_never_panic() {
        for (bytes, class) in [(LIVE_KEYBOARD, DeviceClass::Keyboard), (LIVE_MOUSE, DeviceClass::Mouse), (LIVE_LINK, DeviceClass::ChromaLink)] {
            let flipped: Vec<u8> = bytes.iter().map(|&x| !x).collect();
            let mut offset = 0;
            while offset < bytes.len() {
                let mut spliced = bytes.to_vec();
                spliced[offset..].copy_from_slice(&flipped[offset..]);
                assert_bounded(&spliced, class);
                let mut zeroed = bytes.to_vec();
                zeroed[offset..].iter_mut().for_each(|b| *b = 0);
                assert_bounded(&zeroed, class);
                offset += 64;
            }
        }
    }

    #[test]
    fn random_corruption_never_panics() {
        let mut rng = Lcg(0xC0FFEE);
        for class in DeviceClass::ALL {
            let len = RECORD0 + RING_DEPTH * class.layout().stride;
            for _ in 0..60 {
                let mut b = match class {
                    DeviceClass::Keyboard => LIVE_KEYBOARD.to_vec(),
                    _ => vec![0u8; len],
                };
                for _ in 0..=rng.next_below(32) {
                    let pos = rng.next_below(b.len());
                    b[pos] = (rng.next_u64() >> 56) as u8;
                }
                assert_bounded(&b, class);
            }
        }
    }
}
