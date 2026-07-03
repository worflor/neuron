//! OBS Studio integration via obs-websocket v5 — the one adapter that
//! connects OUT (neuron is the client) rather than serving.
//!
//! This is the honest replacement for the fake-keystroke hack the research
//! found streamers using: instead of a macro sending a phantom hotkey that
//! OBS's global-hotkey listener might catch (fragile, focus-dependent, steals
//! the key everywhere), neuron opens a real websocket to `localhost:4455`,
//! authenticates once, and issues typed requests — scene switches that work
//! minimized, with no key collisions — AND *listens* for OBS's own events
//! (stream went live, mic muted, scene changed) which land on the signal bus
//! for the compositor and macro engine to react to. Parity + bidirectional
//! truth, the four-question bar.
//!
//! The wire protocol (obs-websocket v5, verified against its protocol.md):
//! - server → `Hello` (op 0): rpcVersion, optional `authentication`{challenge,
//!   salt} present only when the user set a password;
//! - client → `Identify` (op 1): rpcVersion, `authentication` string (see
//!   [`crate::crypto::obs_auth`]) iff Hello carried one, an
//!   `eventSubscriptions` bitmask;
//! - server → `Identified` (op 2): the session is live;
//! - client → `Request` (op 6) {requestType, requestId, requestData};
//!   server → `RequestResponse` (op 7); server → `Event` (op 5).
//!
//! This module is a PURE message state machine: JSON text in
//! ([`ObsClient::on_message`]), JSON text out (to send) + [`ObsEvent`]s (to
//! publish). No socket, no threads, no clock — the pump in `net.rs` owns those
//! and stays dumb, so the handshake and event mapping are unit-tested with
//! plain strings.

use crate::crypto::obs_auth;

/// obs-websocket opcodes.
mod op {
    pub const HELLO: u64 = 0;
    pub const IDENTIFY: u64 = 1;
    pub const IDENTIFIED: u64 = 2;
    pub const EVENT: u64 = 5;
    pub const REQUEST: u64 = 6;
    pub const REQUEST_RESPONSE: u64 = 7;
}

/// `eventSubscriptions` bitmask bits we care about (General | Inputs | Scenes |
/// Outputs = 1|4|8|64). Requesting only what we map keeps OBS from firing a
/// firehose we'd ignore.
const EVENT_SUBS: u64 = 1 | 4 | 8 | 64;

/// A meaningful OBS event, normalized for the signal bus. The pump publishes
/// these as retained signals (`obs.streaming`, `obs.scene`, …) so a macro or
/// lighting layer can react to real broadcast state.
#[derive(Clone, Debug, PartialEq)]
pub enum ObsEvent {
    Streaming(bool),
    Recording(bool),
    Scene(String),
    InputMute { name: String, muted: bool },
}

/// What the pump should do after feeding a message in.
#[derive(Debug, Default, PartialEq)]
pub struct Step {
    /// Frames to send back to OBS, in order (the Identify handshake; the
    /// status-resync requests right after Identified).
    pub send: Vec<String>,
    /// Events to publish on the bus.
    pub events: Vec<ObsEvent>,
    /// The session just became fully identified (ready for requests).
    pub identified: bool,
}

/// The connection state machine. One per OBS websocket connection.
pub struct ObsClient {
    password: String,
    pub identified: bool,
    /// Monotonic request-id source (echoed in RequestResponse; we don't yet
    /// correlate responses, but a stable id is protocol-correct).
    next_req: u64,
}

impl ObsClient {
    /// `password` empty = a passwordless OBS (skip the auth string).
    pub fn new(password: impl Into<String>) -> ObsClient {
        ObsClient { password: password.into(), identified: false, next_req: 1 }
    }

    /// Feed one text frame from OBS; get back what to send + events to publish.
    /// Tolerant: anything unparseable or unrecognized yields an empty step
    /// (a future OBS opcode can't wedge us).
    pub fn on_message(&mut self, text: &str) -> Step {
        let Some(v) = parse_json(text) else { return Step::default() };
        match json_u64(&v, "op") {
            Some(op::HELLO) => self.on_hello(&v),
            Some(op::IDENTIFIED) => {
                self.identified = true;
                // RESYNC: events only announce CHANGES, so a (re)connect while a
                // stream is already live would sit on stale defaults until the
                // next transition. Ask for the truth at handshake — the parity
                // question of the four-question bar, answered up front.
                Step {
                    identified: true,
                    send: self.resync_requests(),
                    ..Default::default()
                }
            }
            Some(op::EVENT) => Step {
                events: self.on_event(&v).into_iter().collect(),
                ..Default::default()
            },
            // RequestResponse: control requests are fire-and-forget, but the
            // resync trio issued on Identified answers here — map those into
            // the same normalized events a live change would produce.
            Some(op::REQUEST_RESPONSE) => Step {
                events: self.on_response(&v).into_iter().collect(),
                ..Default::default()
            },
            _ => Step::default(),
        }
    }

    /// The status-resync trio: ask OBS to re-announce the truth its events only
    /// ever deliver as changes. Sent at Identified, and again when the consumer's
    /// world was reborn underneath a still-live connection (the app's bus loses
    /// its retained `obs.*` values on kernel rebirth — see `ObsCmd::Resync`).
    /// One builder so both paths request the exact same truth.
    pub fn resync_requests(&mut self) -> Vec<String> {
        vec![
            self.request("GetStreamStatus", ""),
            self.request("GetRecordStatus", ""),
            self.request("GetCurrentProgramScene", ""),
        ]
    }

    fn on_hello(&mut self, v: &Json) -> Step {
        // `d.authentication` is present only when OBS has a password set.
        let auth_str = json_get(v, "d")
            .and_then(|d| json_get(d, "authentication"))
            .map(|a| {
                let challenge = json_str(a, "challenge").unwrap_or_default();
                let salt = json_str(a, "salt").unwrap_or_default();
                obs_auth(&self.password, &salt, &challenge)
            });
        let identify = build_identify(auth_str.as_deref());
        Step { send: vec![identify], ..Default::default() }
    }

    /// Map a RequestResponse (op 7) to an event, for the resync trio. A failed
    /// request carries no `responseData`, so every getter yields None → inert.
    fn on_response(&self, v: &Json) -> Option<ObsEvent> {
        let d = json_get(v, "d")?;
        let kind = json_str(d, "requestType")?;
        let data = json_get(d, "responseData");
        match kind.as_str() {
            "GetStreamStatus" => {
                Some(ObsEvent::Streaming(data.and_then(|x| json_bool(x, "outputActive"))?))
            }
            "GetRecordStatus" => {
                Some(ObsEvent::Recording(data.and_then(|x| json_bool(x, "outputActive"))?))
            }
            "GetCurrentProgramScene" => {
                // 5.0 names it currentProgramSceneName; 5.4+ also sends sceneName.
                let data = data?;
                Some(ObsEvent::Scene(
                    json_str(data, "currentProgramSceneName")
                        .or_else(|| json_str(data, "sceneName"))?,
                ))
            }
            _ => None,
        }
    }

    fn on_event(&self, v: &Json) -> Option<ObsEvent> {
        let d = json_get(v, "d")?;
        let kind = json_str(d, "eventType")?;
        let data = json_get(d, "eventData");
        match kind.as_str() {
            "StreamStateChanged" => {
                Some(ObsEvent::Streaming(data.and_then(|x| json_bool(x, "outputActive"))?))
            }
            "RecordStateChanged" => {
                Some(ObsEvent::Recording(data.and_then(|x| json_bool(x, "outputActive"))?))
            }
            "CurrentProgramSceneChanged" => {
                Some(ObsEvent::Scene(data.and_then(|x| json_str(x, "sceneName"))?))
            }
            "InputMuteStateChanged" => {
                let data = data?;
                Some(ObsEvent::InputMute {
                    name: json_str(data, "inputName")?,
                    muted: json_bool(data, "inputMuted")?,
                })
            }
            _ => None,
        }
    }

    /// Build a `Request` (op 6) frame — a scene switch, stream toggle, etc.
    /// The pump sends the returned string. `data` is a JSON object literal
    /// (or "{}" for none). Only valid once [`identified`](Self::identified).
    pub fn request(&mut self, request_type: &str, data: &str) -> String {
        let id = self.next_req;
        self.next_req += 1;
        format!(
            r#"{{"op":{},"d":{{"requestType":{},"requestId":"neuron-{id}","requestData":{}}}}}"#,
            op::REQUEST,
            json_string(request_type),
            if data.trim().is_empty() { "{}" } else { data },
        )
    }

    /// Convenience: switch the active program scene.
    pub fn set_scene(&mut self, scene: &str) -> String {
        self.request(
            "SetCurrentProgramScene",
            &format!(r#"{{"sceneName":{}}}"#, json_string(scene)),
        )
    }
}

fn build_identify(auth: Option<&str>) -> String {
    match auth {
        Some(a) => format!(
            r#"{{"op":{},"d":{{"rpcVersion":1,"authentication":{},"eventSubscriptions":{}}}}}"#,
            op::IDENTIFY,
            json_string(a),
            EVENT_SUBS
        ),
        None => format!(
            r#"{{"op":{},"d":{{"rpcVersion":1,"eventSubscriptions":{}}}}}"#,
            op::IDENTIFY,
            EVENT_SUBS
        ),
    }
}

// ── A tiny read-only JSON reader ──────────────────────────────────────────
// neuron-host is zero-dep; the app pulls serde_json, but the kernel doesn't,
// and obs frames are small + regular. This reads just what the handshake and
// events need (objects, strings, bools, numbers). It is NOT a general parser —
// it's deliberately minimal, and anything it can't read yields None (which the
// state machine treats as "ignore"), so a malformed or novel frame is inert,
// never a panic.

#[derive(Debug, Clone, PartialEq)]
enum Json {
    Null,
    Bool(bool),
    Num(f64),
    Str(String),
    Arr(Vec<Json>),
    Obj(Vec<(String, Json)>),
}

fn parse_json(s: &str) -> Option<Json> {
    let bytes = s.as_bytes();
    let mut i = 0;
    let v = parse_value(bytes, &mut i)?;
    skip_ws(bytes, &mut i);
    Some(v) // trailing bytes tolerated — obs sends one frame per message
}

fn skip_ws(b: &[u8], i: &mut usize) {
    while *i < b.len() && matches!(b[*i], b' ' | b'\t' | b'\n' | b'\r') {
        *i += 1;
    }
}

fn parse_value(b: &[u8], i: &mut usize) -> Option<Json> {
    skip_ws(b, i);
    match b.get(*i)? {
        b'{' => parse_obj(b, i),
        b'[' => parse_arr(b, i),
        b'"' => parse_str(b, i).map(Json::Str),
        b't' => lit(b, i, "true", Json::Bool(true)),
        b'f' => lit(b, i, "false", Json::Bool(false)),
        b'n' => lit(b, i, "null", Json::Null),
        _ => parse_num(b, i),
    }
}

fn lit(b: &[u8], i: &mut usize, word: &str, val: Json) -> Option<Json> {
    if b[*i..].starts_with(word.as_bytes()) {
        *i += word.len();
        Some(val)
    } else {
        None
    }
}

fn parse_obj(b: &[u8], i: &mut usize) -> Option<Json> {
    *i += 1; // {
    let mut out = Vec::new();
    loop {
        skip_ws(b, i);
        match b.get(*i)? {
            b'}' => {
                *i += 1;
                return Some(Json::Obj(out));
            }
            b',' => *i += 1,
            b'"' => {
                let key = parse_str(b, i)?;
                skip_ws(b, i);
                if *b.get(*i)? != b':' {
                    return None;
                }
                *i += 1;
                let val = parse_value(b, i)?;
                out.push((key, val));
            }
            _ => return None,
        }
    }
}

fn parse_arr(b: &[u8], i: &mut usize) -> Option<Json> {
    *i += 1; // [
    let mut out = Vec::new();
    loop {
        skip_ws(b, i);
        match b.get(*i)? {
            b']' => {
                *i += 1;
                return Some(Json::Arr(out));
            }
            b',' => *i += 1,
            _ => out.push(parse_value(b, i)?),
        }
    }
}

fn parse_str(b: &[u8], i: &mut usize) -> Option<String> {
    *i += 1; // opening quote
    // Accumulate BYTES, decode as UTF-8 once at the closing quote. Pushing raw
    // bytes as `char`s would reinterpret UTF-8 code units as Latin-1 code
    // points and mangle every non-ASCII scene/input name.
    let mut s: Vec<u8> = Vec::new();
    loop {
        let c = *b.get(*i)?;
        *i += 1;
        match c {
            b'"' => return Some(String::from_utf8_lossy(&s).into_owned()),
            b'\\' => {
                let e = *b.get(*i)?;
                *i += 1;
                match e {
                    b'"' => s.push(b'"'),
                    b'\\' => s.push(b'\\'),
                    b'/' => s.push(b'/'),
                    b'n' => s.push(b'\n'),
                    b't' => s.push(b'\t'),
                    b'r' => s.push(b'\r'),
                    b'b' => s.push(0x08),
                    b'f' => s.push(0x0c),
                    b'u' => {
                        let ch = parse_u_escape(b, i)?;
                        let mut buf = [0u8; 4];
                        s.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
                    }
                    _ => return None,
                }
            }
            // Raw bytes — including multi-byte UTF-8 sequences — pass through untouched.
            _ => s.push(c),
        }
    }
}

/// Decode one `\uXXXX` escape (the leading `\u` already consumed), including
/// JSON's UTF-16 surrogate-pair encoding of astral characters (a high half
/// like `\uD83D` followed by a low half like `\uDE00` decodes to ONE emoji).
/// A lone/malformed surrogate half yields U+FFFD rather than failing the
/// whole frame — matching the tolerant stance of the byte path above.
fn parse_u_escape(b: &[u8], i: &mut usize) -> Option<char> {
    let hi = hex4(b, i)?;
    if (0xD800..=0xDBFF).contains(&hi) {
        // High surrogate: only meaningful paired with a following `\uDC00..DFFF`.
        if b.get(*i) == Some(&b'\\') && b.get(*i + 1) == Some(&b'u') {
            let save = *i;
            *i += 2;
            match hex4(b, i) {
                Some(lo) if (0xDC00..=0xDFFF).contains(&lo) => {
                    let cp = 0x10000 + ((hi - 0xD800) << 10) + (lo - 0xDC00);
                    return char::from_u32(cp); // always valid by construction
                }
                // Not a low surrogate — rewind so the next escape parses on its own.
                _ => *i = save,
            }
        }
        return Some('\u{fffd}'); // lone high surrogate
    }
    if (0xDC00..=0xDFFF).contains(&hi) {
        return Some('\u{fffd}'); // lone low surrogate
    }
    char::from_u32(hi) // BMP scalar — always valid outside the surrogate range
}

/// Four hex digits at `*i` → code unit; advances past them on success.
fn hex4(b: &[u8], i: &mut usize) -> Option<u32> {
    let hex = b.get(*i..*i + 4)?;
    let cp = u32::from_str_radix(std::str::from_utf8(hex).ok()?, 16).ok()?;
    *i += 4;
    Some(cp)
}

fn parse_num(b: &[u8], i: &mut usize) -> Option<Json> {
    let start = *i;
    while *i < b.len() && matches!(b[*i], b'0'..=b'9' | b'-' | b'+' | b'.' | b'e' | b'E') {
        *i += 1;
    }
    std::str::from_utf8(&b[start..*i]).ok()?.parse().ok().map(Json::Num)
}

fn json_get<'a>(v: &'a Json, key: &str) -> Option<&'a Json> {
    match v {
        Json::Obj(pairs) => pairs.iter().find(|(k, _)| k == key).map(|(_, val)| val),
        _ => None,
    }
}

fn json_str(v: &Json, key: &str) -> Option<String> {
    match json_get(v, key)? {
        Json::Str(s) => Some(s.clone()),
        _ => None,
    }
}

fn json_bool(v: &Json, key: &str) -> Option<bool> {
    match json_get(v, key)? {
        Json::Bool(b) => Some(*b),
        _ => None,
    }
}

fn json_u64(v: &Json, key: &str) -> Option<u64> {
    match json_get(v, key)? {
        Json::Num(n) => Some(*n as u64),
        _ => None,
    }
}

/// Minimal JSON string-escaping for values we emit (scene/input names).
fn json_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            _ => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn passwordless_hello_identifies_without_auth() {
        let mut c = ObsClient::new("");
        let step = c.on_message(r#"{"op":0,"d":{"rpcVersion":1}}"#);
        let sent = step.send.first().expect("must reply with Identify");
        assert!(sent.contains(r#""op":1"#));
        assert!(!sent.contains("authentication"), "no password ⇒ no auth string");
        assert!(sent.contains("eventSubscriptions"));
    }

    #[test]
    fn password_hello_includes_the_auth_string() {
        let mut c = ObsClient::new("supersecretpassword");
        let hello = r#"{"op":0,"d":{"rpcVersion":1,"authentication":{"challenge":"TO+ZS6d1UZKS8xhXV9YCS1MyJb3XkS+FzY8ceKcTFwc=","salt":"lM1GncleQOaCu9lT1yeUZhFYnqhsLLP1G5lAGo3ixaI="}}}"#;
        let step = c.on_message(hello);
        let sent = step.send.first().expect("Identify");
        let expect = obs_auth(
            "supersecretpassword",
            "lM1GncleQOaCu9lT1yeUZhFYnqhsLLP1G5lAGo3ixaI=",
            "TO+ZS6d1UZKS8xhXV9YCS1MyJb3XkS+FzY8ceKcTFwc=",
        );
        assert!(sent.contains(&expect), "the computed auth string must be in Identify");
    }

    #[test]
    fn identified_flips_ready_and_requests_a_status_resync() {
        let mut c = ObsClient::new("");
        assert!(!c.identified);
        let step = c.on_message(r#"{"op":2,"d":{"negotiatedRpcVersion":1}}"#);
        assert!(step.identified);
        assert!(c.identified);
        // The resync trio: without it, connecting mid-stream believes
        // streaming=false until the next transition (a tally that lies).
        let all = step.send.join("\n");
        for want in ["GetStreamStatus", "GetRecordStatus", "GetCurrentProgramScene"] {
            assert!(all.contains(want), "resync must request {want}");
        }
    }

    #[test]
    fn resync_responses_map_to_the_same_events_as_live_changes() {
        let mut c = ObsClient::new("");
        let stream = r#"{"op":7,"d":{"requestType":"GetStreamStatus","requestId":"neuron-1","requestStatus":{"result":true,"code":100},"responseData":{"outputActive":true,"outputDuration":120000}}}"#;
        assert_eq!(c.on_message(stream).events, vec![ObsEvent::Streaming(true)]);
        let rec = r#"{"op":7,"d":{"requestType":"GetRecordStatus","requestId":"neuron-2","requestStatus":{"result":true,"code":100},"responseData":{"outputActive":false}}}"#;
        assert_eq!(c.on_message(rec).events, vec![ObsEvent::Recording(false)]);
        let scene = r#"{"op":7,"d":{"requestType":"GetCurrentProgramScene","requestId":"neuron-3","requestStatus":{"result":true,"code":100},"responseData":{"currentProgramSceneName":"Gameplay"}}}"#;
        assert_eq!(c.on_message(scene).events, vec![ObsEvent::Scene("Gameplay".into())]);
        // A FAILED request has no responseData — must stay inert, never a panic.
        let failed = r#"{"op":7,"d":{"requestType":"GetStreamStatus","requestId":"neuron-4","requestStatus":{"result":false,"code":604}}}"#;
        assert_eq!(c.on_message(failed), Step::default());
        // A control-request ack (SetCurrentProgramScene etc.) is not an event.
        let ack = r#"{"op":7,"d":{"requestType":"SetCurrentProgramScene","requestId":"neuron-5","requestStatus":{"result":true,"code":100}}}"#;
        assert_eq!(c.on_message(ack), Step::default());
    }

    #[test]
    fn stream_and_record_events_map() {
        let mut c = ObsClient::new("");
        let go_live = r#"{"op":5,"d":{"eventType":"StreamStateChanged","eventData":{"outputActive":true,"outputState":"OBS_WEBSOCKET_OUTPUT_STARTED"}}}"#;
        assert_eq!(c.on_message(go_live).events, vec![ObsEvent::Streaming(true)]);
        let rec_off = r#"{"op":5,"d":{"eventType":"RecordStateChanged","eventData":{"outputActive":false}}}"#;
        assert_eq!(c.on_message(rec_off).events, vec![ObsEvent::Recording(false)]);
    }

    #[test]
    fn scene_and_mute_events_map() {
        let mut c = ObsClient::new("");
        let scene = r#"{"op":5,"d":{"eventType":"CurrentProgramSceneChanged","eventData":{"sceneName":"Gameplay"}}}"#;
        assert_eq!(c.on_message(scene).events, vec![ObsEvent::Scene("Gameplay".into())]);
        let mute = r#"{"op":5,"d":{"eventType":"InputMuteStateChanged","eventData":{"inputName":"Mic/Aux","inputMuted":true}}}"#;
        assert_eq!(
            c.on_message(mute).events,
            vec![ObsEvent::InputMute { name: "Mic/Aux".into(), muted: true }]
        );
    }

    #[test]
    fn unknown_events_and_garbage_are_inert() {
        let mut c = ObsClient::new("");
        assert_eq!(c.on_message(r#"{"op":5,"d":{"eventType":"SomeFutureEvent"}}"#), Step::default());
        assert_eq!(c.on_message("not json at all {{{"), Step::default());
        assert_eq!(c.on_message(r#"{"op":999}"#), Step::default());
        assert_eq!(c.on_message(""), Step::default());
    }

    #[test]
    fn set_scene_builds_a_valid_request_frame() {
        let mut c = ObsClient::new("");
        let f = c.set_scene("Just Chatting");
        assert!(f.contains(r#""op":6"#));
        assert!(f.contains(r#""requestType":"SetCurrentProgramScene""#));
        assert!(f.contains(r#""sceneName":"Just Chatting""#));
        // and it must be parseable back (round-trip through our own reader)
        assert!(parse_json(&f).is_some(), "emitted request must be valid JSON");
    }

    #[test]
    fn scene_names_with_quotes_are_escaped() {
        let mut c = ObsClient::new("");
        let f = c.set_scene(r#"He said "hi""#);
        assert!(parse_json(&f).is_some(), "escaping must keep the frame valid JSON");
        // and the escaped payload round-trips back to the original name
        let v = parse_json(&f).unwrap();
        let name = json_get(&v, "d")
            .and_then(|d| json_get(d, "requestData"))
            .and_then(|rd| json_str(rd, "sceneName"));
        assert_eq!(name.as_deref(), Some(r#"He said "hi""#));
    }

    #[test]
    fn resync_requests_asks_for_the_same_truth_as_identify() {
        // The rebirth path (`ObsCmd::Resync`) and the identify path must request the
        // exact same status truth — one builder serves both, and this pins the trio.
        let mut c = ObsClient::new("");
        let all = c.resync_requests().join("\n");
        for want in ["GetStreamStatus", "GetRecordStatus", "GetCurrentProgramScene"] {
            assert!(all.contains(want), "resync must request {want}");
        }
        // and each frame is a well-formed request our own reader accepts.
        for frame in c.resync_requests() {
            assert!(parse_json(&frame).is_some(), "resync frame must be valid JSON");
        }
    }

    #[test]
    fn non_ascii_names_survive_the_reader_intact() {
        // Raw UTF-8 bytes on the wire — the shape OBS actually sends (it does
        // not \u-escape by default). Umlauts, CJK, and an astral emoji all
        // cross the parser unmangled.
        for name in ["Szene Überblick", "配信画面", "Café — späti", "Go Live 🎥🔴"] {
            let frame = format!(
                r#"{{"op":5,"d":{{"eventType":"CurrentProgramSceneChanged","eventData":{{"sceneName":{}}}}}}}"#,
                json_string(name)
            );
            let mut c = ObsClient::new("");
            assert_eq!(
                c.on_message(&frame).events,
                vec![ObsEvent::Scene(name.into())],
                "name {name:?} must round-trip"
            );
        }
    }

    #[test]
    fn unicode_escapes_decode_including_surrogate_pairs() {
        // Escapes are assembled at runtime so this source stays ASCII and the
        // test provably exercises the \u path, not the raw-byte path.
        // BMP escape: u00dc decodes to Ü
        let bmp = String::from(r#"{"n":""#) + "\\u00dcberblick" + r#""}"#;
        let v = parse_json(&bmp).unwrap();
        assert_eq!(json_str(&v, "n").as_deref(), Some("Überblick"));
        // Astral char as a JSON surrogate pair (how escaping encoders emit emoji):
        // 🎥 = U+1F3A5 (movie camera)
        let astral = String::from(r#"{"n":"live "#) + "\\ud83c\\udfa5" + r#" now"}"#;
        let v = parse_json(&astral).unwrap();
        assert_eq!(json_str(&v, "n").as_deref(), Some("live \u{1F3A5} now"));
        // A lone surrogate half is tolerated as U+FFFD, never a corrupt string or a dropped frame.
        let v = parse_json(r#"{"n":"bad \ud83c half"}"#).unwrap();
        assert_eq!(json_str(&v, "n").as_deref(), Some("bad \u{fffd} half"));
    }

    #[test]
    fn non_ascii_names_round_trip_through_emit_and_reparse() {
        // The mute-hook path: an internationalized input name must survive
        // emit (json_string) → wire → reader, or `obs.mute.<name>` bus keys
        // and hook macros would misaddress the input.
        let mut c = ObsClient::new("");
        let f = c.set_scene("配信 — Überblick 🎬");
        let v = parse_json(&f).expect("emitted frame stays valid JSON");
        let name = json_get(&v, "d")
            .and_then(|d| json_get(d, "requestData"))
            .and_then(|rd| json_str(rd, "sceneName"));
        assert_eq!(name.as_deref(), Some("配信 — Überblick 🎬"));
    }

    #[test]
    fn nested_json_reader_handles_the_real_hello_shape() {
        // exercise objects-in-objects, the exact structure on_hello walks
        let v = parse_json(r#"{"op":0,"d":{"obsWebSocketVersion":"5.4.2","rpcVersion":1,"authentication":{"challenge":"abc","salt":"def"}}}"#).unwrap();
        let auth = json_get(&v, "d").and_then(|d| json_get(d, "authentication")).unwrap();
        assert_eq!(json_str(auth, "challenge").as_deref(), Some("abc"));
        assert_eq!(json_str(auth, "salt").as_deref(), Some("def"));
    }
}
