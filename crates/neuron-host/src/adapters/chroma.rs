//! Chroma SDK REST server adapter — the flagship port.
//!
//! Chroma-enabled games and tools open a session against `localhost:54235`, heartbeat
//! it every ~1s, and drive lighting effects per device. This is the REST face; native
//! games that speak shared memory instead are served by [`super::chroma_shm`]. Together
//! they catch both, with nothing injected into any game process.
//!
//! This module is a PURE request→response state machine: no sockets, no
//! threads, no clock reads — the HTTP pump lives elsewhere and time arrives
//! as a parameter. Session teardown needs no timers: every session layer is
//! claimed with a 15-second TTL lease, and every effect write or heartbeat
//! refreshes it. A game that crashes simply stops refreshing, its lease
//! lapses, and the user's base lighting returns — the protocol's own 15s
//! inactivity contract, enforced by the arbiter instead of by cleanup code
//! (§5.1: teardown is the default path).
//!
//! ## Conformance status (two adversarial audit passes, primary sources)
//! Verified MATCH: endpoints + ~1s heartbeat + 15s timeout (Razer REST portal
//! index), BGR/COLORREF decode (python-chroma-rest-server `Color.from_long_bgr`),
//! effect names + param shapes incl. keyboard CUSTOM2's OBJECT param
//! (`{"color": 8×24, "key": 6×22}` — keyboard docs) vs mouse CUSTOM2's flat
//! 9×7 array (mouse docs), batch `{"effects": [...]}` bodies
//! (python-chroma-rest-server `resource.py`), RZRESULT codes 0/87/1168/4319
//! (`RzErrors.h`), init reply field names (`PostChromaSdkResponse`: Sessionid,
//! Uri — we reply a superset).
//!
//! Deliberate decisions where sources disagree (capture/replay will settle):
//! - **POST stores an effect (returns `id`) without applying; PUT applies.**
//!   The official SDK's create/apply split (UnityChromaSDK flow) — and a
//!   preload-style client creating many effects up front must not flash each
//!   one on creation. python-chroma-rest-server applies on POST; we follow
//!   the SDK contract instead.
//! - Firmware effect names over REST (CHROMA_WAVE etc.): the official REST
//!   device pages document only NONE/STATIC/CUSTOM/CUSTOM_KEY/CUSTOM2, so we
//!   refuse others with INVALID_PARAMETER rather than fake them.
//! - Session ids are minted in a port-plausible range (≥54236): the official
//!   init reply's sessionid doubles as a per-session PORT in some client
//!   flows; a future pump can bind those ports, and ids like 1,2,3 would be
//!   maximally wrong for such clients. We also accept paths with or without
//!   the `/razer` prefix.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use crate::api::{HostApi, LeaseSpec, SurfaceInfo, SurfaceKind};
use crate::arbiter::{band, Content, LayerId, Rgb, SourceId};
use crate::bus::Value;

/// The Chroma SDK's own session contract: 15s of silence = dead. Layer leases
/// AND session bookkeeping both use it, so a stalled game's session dies at
/// the same moment its paint does (parity with the real server, which refuses
/// the session after the timeout — the app must re-init).
pub const SESSION_TTL: Duration = Duration::from_secs(15);

/// First minted session id — in port-space above the SDK's own 54235 (see
/// module docs).
const FIRST_SESSION_ID: u64 = 54236;

/// RZRESULT codes (Windows error codes, as the SDK reuses them — RzErrors.h).
mod rz {
    pub const SUCCESS: i64 = 0;
    pub const INVALID_PARAMETER: i64 = 87;
    pub const NOT_FOUND: i64 = 1168;
    /// "Device not available or supported" — the correct code when no surface
    /// of the requested kind exists (audit: python-chroma-rest-server maps its
    /// no-device case to exactly this).
    pub const DEVICE_NOT_AVAILABLE: i64 = 4319;
}

pub struct HttpRequest {
    /// "GET" | "POST" | "PUT" | "DELETE" (uppercase).
    pub method: String,
    /// Path only, e.g. "/razer/chromasdk/sess/54236/keyboard".
    pub path: String,
    pub body: Vec<u8>,
}

pub struct HttpResponse {
    pub status: u16,
    /// JSON body.
    pub body: String,
}

impl HttpResponse {
    fn json(status: u16, body: serde_json::Value) -> HttpResponse {
        HttpResponse { status, body: body.to_string() }
    }

    fn result(code: i64) -> HttpResponse {
        HttpResponse::json(200, serde_json::json!({ "result": code }))
    }

    fn err(status: u16, code: i64) -> HttpResponse {
        HttpResponse::json(status, serde_json::json!({ "result": code }))
    }
}

/// What a stored/applied effect does to its device.
#[derive(Clone)]
enum Action {
    /// CHROMA_NONE: release the session's layer on that device.
    Clear,
    Paint(Content),
}

#[derive(Clone)]
struct StoredEffect {
    device: String,
    action: Action,
}

/// One session's live paint on one device: the arbiter layer plus the surface
/// key and last content it was claimed with. The key + content are RETAINED so
/// a kernel rebirth (which sweeps every lease) can be recovered on the next
/// heartbeat — re-claiming identical paint — without waiting for the game to
/// push a fresh effect. See [`ChromaServer::heartbeat`].
struct DeviceLayer {
    layer: LayerId,
    key: String,
    content: Content,
}

struct Session {
    owner: SourceId,
    /// device endpoint ("keyboard", …) → its live layer.
    layers: HashMap<String, DeviceLayer>,
    /// POST-created effects awaiting PUT-apply, keyed by minted id.
    effects: HashMap<String, StoredEffect>,
    last_seen: Instant,
    title: String,
    heartbeats: u64,
}

/// The Chroma REST server state machine. One instance serves all sessions.
pub struct ChromaServer {
    sessions: HashMap<u64, Session>,
    next_id: u64,
    next_effect: u64,
}

impl ChromaServer {
    pub fn new() -> ChromaServer {
        ChromaServer { sessions: HashMap::new(), next_id: FIRST_SESSION_ID, next_effect: 1 }
    }

    /// The LIVE sessions (owner + game title), for the host's status readout.
    /// TTL-filtered with the same 15s contract the prune uses, so a vanished
    /// game stops being REPORTED at the same moment its paint expires — even
    /// before the next request-driven prune actually sweeps the entry.
    pub fn sessions(&self, now: Instant) -> Vec<(SourceId, String)> {
        self.sessions
            .values()
            .filter(|s| now.duration_since(s.last_seen) <= SESSION_TTL)
            .map(|s| (s.owner, s.title.clone()))
            .collect()
    }

    pub fn handle(
        &mut self,
        req: &HttpRequest,
        host: &mut dyn HostApi,
        now: Instant,
    ) -> HttpResponse {
        self.prune(host, now);
        let segs: Vec<&str> = req.path.split('/').filter(|s| !s.is_empty()).collect();
        // Root: "/razer/chromasdk" (canonical) or "/chromasdk" (the shape the
        // official init reply's uri uses).
        let root = matches!(segs.as_slice(), ["razer", "chromasdk"] | ["chromasdk"]);
        if root {
            return match req.method.as_str() {
                "GET" => HttpResponse::json(
                    200,
                    serde_json::json!({
                        "result": rz::SUCCESS,
                        // Chroma-plausible version string; our real identity
                        // rides alongside so nothing is misrepresented.
                        "version": "3.0",
                        "core": concat!("neuron-host ", env!("CARGO_PKG_VERSION")),
                    }),
                ),
                "POST" => self.init(req, host, now),
                _ => HttpResponse::err(400, rz::INVALID_PARAMETER),
            };
        }
        // Session ops: any path containing ".../sess/{id}[/{rest}]".
        if let Some(pos) = segs.iter().position(|s| *s == "sess") {
            let Some(id) = segs.get(pos + 1).and_then(|s| s.parse::<u64>().ok()) else {
                return HttpResponse::err(404, rz::NOT_FOUND);
            };
            let rest = &segs[pos + 2..];
            return match (req.method.as_str(), rest) {
                ("GET", []) => self.session_info(id),
                ("DELETE", []) => self.uninit(id, host),
                ("PUT", ["heartbeat"]) => self.heartbeat(id, host, now),
                ("PUT", ["effect"]) => self.apply_stored(id, &req.body, host, now),
                ("DELETE", ["effect"]) => self.free_stored(id, &req.body, now),
                ("POST", [device]) => self.create(id, device, &req.body, host, now),
                ("PUT", [device]) => self.apply_now(id, device, &req.body, host, now),
                _ => HttpResponse::err(404, rz::NOT_FOUND),
            };
        }
        HttpResponse::err(404, rz::NOT_FOUND)
    }

    /// `POST /razer/chromasdk` — open a session for an app.
    fn init(&mut self, req: &HttpRequest, host: &mut dyn HostApi, now: Instant) -> HttpResponse {
        let Ok(info) = serde_json::from_slice::<serde_json::Value>(&req.body) else {
            return HttpResponse::err(400, rz::INVALID_PARAMETER);
        };
        let title = info
            .get("title")
            .and_then(|t| t.as_str())
            .unwrap_or("unnamed chroma app")
            .to_string();
        let id = self.next_id;
        self.next_id += 1;
        let owner = host.next_source();
        // Name the source NOW — the GUI's ownership truth reads "Overwatch is
        // painting this board", not "another app".
        host.label_source(owner, &title);
        self.sessions.insert(
            id,
            Session {
                owner,
                layers: HashMap::new(),
                effects: HashMap::new(),
                last_seen: now,
                title: title.clone(),
                heartbeats: 0,
            },
        );
        host.publish("host.chroma.session", Value::Text(title));
        // We mint this URI and we parse it — self-consistent by construction.
        HttpResponse::json(
            200,
            serde_json::json!({
                "result": rz::SUCCESS,
                "sessionid": id,
                "session": id,
                "uri": format!("http://localhost:54235/razer/chromasdk/sess/{id}"),
            }),
        )
    }

    fn session_info(&self, id: u64) -> HttpResponse {
        match self.sessions.get(&id) {
            Some(s) => HttpResponse::json(
                200,
                serde_json::json!({
                    "result": rz::SUCCESS,
                    "info": { "title": s.title, "heartbeats": s.heartbeats },
                }),
            ),
            None => HttpResponse::err(404, rz::NOT_FOUND),
        }
    }

    fn uninit(&mut self, id: u64, host: &mut dyn HostApi) -> HttpResponse {
        match self.sessions.remove(&id) {
            Some(s) => {
                host.release_owner(s.owner);
                host.publish("host.chroma.closed", Value::Text(s.title));
                HttpResponse::result(rz::SUCCESS)
            }
            None => HttpResponse::err(404, rz::NOT_FOUND),
        }
    }

    fn heartbeat(&mut self, id: u64, host: &mut dyn HostApi, now: Instant) -> HttpResponse {
        let Some(s) = self.sessions.get_mut(&id) else {
            return HttpResponse::err(404, rz::NOT_FOUND);
        };
        s.last_seen = now;
        s.heartbeats += 1;
        // Refresh each layer's lease — and use that same probe to detect a kernel
        // rebirth. A heartbeating session keeps its own lease well within the 15s
        // TTL, so a layer that `refresh` reports GONE was swept by a contained
        // fault, not a timeout. Re-claim those from the retained content so a game
        // that only heartbeats a static effect doesn't go dark after a rebirth
        // (the old path left it dark until the next effect WRITE, which such a
        // game never sends).
        let dead: Vec<String> = s
            .layers
            .iter()
            .filter(|(_, dl)| !host.refresh(dl.layer, now))
            .map(|(dev, _)| dev.clone())
            .collect();
        if !dead.is_empty() {
            for dev in dead {
                let (key, content) = {
                    let dl = &s.layers[&dev];
                    (dl.key.clone(), dl.content.clone())
                };
                if let Some(layer) = host.claim(
                    &key,
                    s.owner,
                    band::SESSION,
                    LeaseSpec::Ttl(SESSION_TTL),
                    content.clone(),
                    now,
                ) {
                    s.layers.insert(dev, DeviceLayer { layer, key, content });
                }
            }
            // The reborn kernel's labels map is empty — restore this session's.
            host.label_source(s.owner, &s.title);
        }
        HttpResponse::json(200, serde_json::json!({ "result": rz::SUCCESS, "tick": s.heartbeats }))
    }

    /// `PUT …/{device}` — parse and apply immediately (single or batch body).
    fn apply_now(
        &mut self,
        id: u64,
        device: &str,
        body: &[u8],
        host: &mut dyn HostApi,
        now: Instant,
    ) -> HttpResponse {
        let (surface, effects) = match self.parse_request(id, device, body, host) {
            Ok(v) => v,
            Err(resp) => return resp,
        };
        let mut results = Vec::new();
        for action in &effects {
            let code = self.apply(id, device, &surface, action.clone(), host, now);
            results.push(serde_json::json!({ "result": code }));
        }
        if let Some(s) = self.sessions.get_mut(&id) {
            s.last_seen = now;
        }
        match results.len() {
            1 => HttpResponse::json(200, results.pop().unwrap()),
            _ => HttpResponse::json(
                200,
                serde_json::json!({ "result": rz::SUCCESS, "results": results }),
            ),
        }
    }

    /// `POST …/{device}` — store effect(s), return id(s), do NOT apply (the
    /// SDK's create/apply split; see module docs).
    fn create(
        &mut self,
        id: u64,
        device: &str,
        body: &[u8],
        host: &mut dyn HostApi,
        now: Instant,
    ) -> HttpResponse {
        let (_, effects) = match self.parse_request(id, device, body, host) {
            Ok(v) => v,
            Err(resp) => return resp,
        };
        let batch = effects.len() > 1;
        let mut results = Vec::new();
        let mut only_id = String::new();
        {
            let s = self.sessions.get_mut(&id).expect("parse_request checked the session");
            s.last_seen = now;
            for action in effects {
                let eid = format!("neuron-{:08x}", self.next_effect);
                self.next_effect += 1;
                s.effects
                    .insert(eid.clone(), StoredEffect { device: device.to_string(), action });
                results.push(serde_json::json!({ "result": rz::SUCCESS, "id": eid }));
                only_id = eid;
            }
        }
        if batch {
            HttpResponse::json(200, serde_json::json!({ "result": rz::SUCCESS, "results": results }))
        } else {
            HttpResponse::json(200, serde_json::json!({ "result": rz::SUCCESS, "id": only_id }))
        }
    }

    /// `PUT …/effect` — apply previously created effect(s) by id.
    fn apply_stored(
        &mut self,
        id: u64,
        body: &[u8],
        host: &mut dyn HostApi,
        now: Instant,
    ) -> HttpResponse {
        if !self.sessions.contains_key(&id) {
            return HttpResponse::err(404, rz::NOT_FOUND);
        }
        let Some(ids) = parse_effect_ids(body) else {
            return HttpResponse::err(400, rz::INVALID_PARAMETER);
        };
        let mut results = Vec::new();
        for eid in &ids {
            let stored = self.sessions.get(&id).and_then(|s| s.effects.get(eid)).cloned();
            let code = match stored {
                Some(e) => {
                    // The stored effect targets whatever surface serves its
                    // device kind NOW — honest against hotplug between create
                    // and apply.
                    match surface_for(host, &e.device) {
                        Some(surface) => self.apply(id, &e.device, &surface, e.action, host, now),
                        None => rz::DEVICE_NOT_AVAILABLE,
                    }
                }
                None => rz::NOT_FOUND,
            };
            results.push(serde_json::json!({ "id": eid, "result": code }));
        }
        if let Some(s) = self.sessions.get_mut(&id) {
            s.last_seen = now;
        }
        match results.len() {
            1 => HttpResponse::json(200, results.pop().unwrap()),
            _ => HttpResponse::json(
                200,
                serde_json::json!({ "result": rz::SUCCESS, "results": results }),
            ),
        }
    }

    /// `DELETE …/effect` — free stored effect(s) by id.
    fn free_stored(&mut self, id: u64, body: &[u8], now: Instant) -> HttpResponse {
        let Some(s) = self.sessions.get_mut(&id) else {
            return HttpResponse::err(404, rz::NOT_FOUND);
        };
        let Some(ids) = parse_effect_ids(body) else {
            return HttpResponse::err(400, rz::INVALID_PARAMETER);
        };
        s.last_seen = now;
        // Remove every known id even when some are unknown (no short-circuit
        // — a mixed batch must not leave later effects allocated).
        let mut all_known = true;
        for eid in &ids {
            if s.effects.remove(eid).is_none() {
                all_known = false;
            }
        }
        HttpResponse::result(if all_known { rz::SUCCESS } else { rz::NOT_FOUND })
    }

    /// Shared request front: session exists, device kind is served, body
    /// parses into one-or-many actions (batch `{"effects": [...]}` bodies
    /// per the reference server).
    fn parse_request(
        &mut self,
        id: u64,
        device: &str,
        body: &[u8],
        host: &mut dyn HostApi,
    ) -> Result<(SurfaceInfo, Vec<Action>), HttpResponse> {
        let Some(kind) = device_kind(device) else {
            return Err(HttpResponse::err(404, rz::NOT_FOUND));
        };
        if !self.sessions.contains_key(&id) {
            return Err(HttpResponse::err(404, rz::NOT_FOUND));
        }
        let Ok(v) = serde_json::from_slice::<serde_json::Value>(body) else {
            return Err(HttpResponse::err(400, rz::INVALID_PARAMETER));
        };
        // Truthful capability answer: no surface of this kind → say so with
        // the SDK's own code for it, don't fake success (§5.4).
        let Some(surface) = host.surfaces().into_iter().find(|s| s.kind == kind) else {
            return Err(HttpResponse::json(
                200,
                serde_json::json!({ "result": rz::DEVICE_NOT_AVAILABLE }),
            ));
        };
        let items: Vec<&serde_json::Value> = match v.get("effects").and_then(|e| e.as_array()) {
            Some(batch) => batch.iter().collect(),
            None => vec![&v],
        };
        let mut actions = Vec::with_capacity(items.len());
        for item in items {
            match parse_effect(item, &surface) {
                Some(a) => actions.push(a),
                None => return Err(HttpResponse::err(400, rz::INVALID_PARAMETER)),
            }
        }
        Ok((surface, actions))
    }

    /// Apply one action to the session's layer on `device`: set-or-claim; a
    /// swept layer (game stalled past the TTL then resumed) is re-claimed
    /// transparently.
    fn apply(
        &mut self,
        id: u64,
        device: &str,
        surface: &SurfaceInfo,
        action: Action,
        host: &mut dyn HostApi,
        now: Instant,
    ) -> i64 {
        let Some(s) = self.sessions.get_mut(&id) else { return rz::NOT_FOUND };
        match action {
            Action::Clear => {
                if let Some(dl) = s.layers.remove(device) {
                    host.release(dl.layer);
                }
                rz::SUCCESS
            }
            Action::Paint(content) => {
                if let Some(dl) = s.layers.get_mut(device) {
                    if host.set_content(dl.layer, content.clone(), now) {
                        // Keep the retained content current so a rebirth recovery
                        // re-claims what's actually painted, not a stale frame.
                        dl.content = content;
                        return rz::SUCCESS;
                    }
                }
                match host.claim(
                    &surface.key,
                    s.owner,
                    band::SESSION,
                    LeaseSpec::Ttl(SESSION_TTL),
                    content.clone(),
                    now,
                ) {
                    Some(layer) => {
                        s.layers.insert(
                            device.to_string(),
                            DeviceLayer { layer, key: surface.key.clone(), content },
                        );
                        rz::SUCCESS
                    }
                    None => rz::DEVICE_NOT_AVAILABLE,
                }
            }
        }
    }

    /// Drop sessions silent past the TTL — the same 15s the real server
    /// enforces (a stalled game must re-init, matching reference behavior;
    /// its layers already stopped painting at the same instant via the lease).
    fn prune(&mut self, host: &mut dyn HostApi, now: Instant) {
        let dead: Vec<u64> = self
            .sessions
            .iter()
            .filter(|(_, s)| now.duration_since(s.last_seen) > SESSION_TTL)
            .map(|(id, _)| *id)
            .collect();
        for id in dead {
            if let Some(s) = self.sessions.remove(&id) {
                host.release_owner(s.owner);
                host.publish("host.chroma.closed", Value::Text(s.title));
            }
        }
    }
}

impl Default for ChromaServer {
    fn default() -> Self {
        Self::new()
    }
}

fn device_kind(device: &str) -> Option<SurfaceKind> {
    Some(match device {
        "keyboard" => SurfaceKind::Keyboard,
        "mouse" => SurfaceKind::Mouse,
        "mousepad" => SurfaceKind::Mousepad,
        "headset" => SurfaceKind::Headset,
        "keypad" => SurfaceKind::Keypad,
        "chromalink" => SurfaceKind::Generic,
        _ => return None,
    })
}

fn surface_for(host: &mut dyn HostApi, device: &str) -> Option<SurfaceInfo> {
    let kind = device_kind(device)?;
    host.surfaces().into_iter().find(|s| s.kind == kind)
}

/// One effect object → an action. Shapes per the official device docs:
/// - `CHROMA_STATIC`: `param.color` BGR int.
/// - `CHROMA_CUSTOM`: `param` = grid array.
/// - `CHROMA_CUSTOM_KEY`: `param` = `{color: grid, key: grid}` (key-code
///   translation is a later refinement; the color grid is honored).
/// - `CHROMA_CUSTOM2`: keyboard = OBJECT `{color: 8×24, key: 6×22}`; mouse =
///   flat 9×7 ARRAY. We accept either shape (array first, then /param/color)
///   so both device families parse — the audit's top finding.
fn parse_effect(v: &serde_json::Value, surface: &SurfaceInfo) -> Option<Action> {
    let effect = v.get("effect").and_then(|e| e.as_str())?;
    match effect {
        "CHROMA_NONE" => Some(Action::Clear),
        "CHROMA_STATIC" => {
            let color = v.pointer("/param/color").and_then(|c| c.as_u64())?;
            Some(Action::Paint(Content::Fill(bgr(color as u32))))
        }
        "CHROMA_CUSTOM" | "CHROMA_CUSTOM2" | "CHROMA_CUSTOM_KEY" => {
            let grid = v
                .get("param")
                .and_then(parse_grid)
                .or_else(|| v.pointer("/param/color").and_then(parse_grid))?;
            Some(Action::Paint(Content::Cells(grid_to_cells(&grid, surface))))
        }
        // Firmware-side effect names (WAVE/BREATHING/…) are not part of the
        // REST surface per the official device docs; refusing beats faking.
        _ => None,
    }
}

/// `{"id": "..."}` or `{"ids": ["...", ...]}`.
fn parse_effect_ids(body: &[u8]) -> Option<Vec<String>> {
    let v = serde_json::from_slice::<serde_json::Value>(body).ok()?;
    if let Some(one) = v.get("id").and_then(|i| i.as_str()) {
        return Some(vec![one.to_string()]);
    }
    let ids = v.get("ids")?.as_array()?;
    ids.iter().map(|i| i.as_str().map(String::from)).collect()
}

/// COLORREF-style BGR int → Rgb (R = v&0xFF, G = v>>8, B = v>>16).
fn bgr(v: u32) -> Rgb {
    Rgb((v & 0xFF) as u8, ((v >> 8) & 0xFF) as u8, ((v >> 16) & 0xFF) as u8)
}

/// Effect param → rows of BGR ints. Accepts a grid (array of arrays) or a
/// flat array (linear devices).
fn parse_grid(v: &serde_json::Value) -> Option<Vec<Vec<u32>>> {
    let arr = v.as_array()?;
    if arr.is_empty() {
        return Some(Vec::new());
    }
    if arr[0].is_array() {
        arr.iter()
            .map(|row| row.as_array()?.iter().map(|c| c.as_u64().map(|c| c as u32)).collect())
            .collect()
    } else {
        Some(vec![arr.iter().filter_map(|c| c.as_u64().map(|c| c as u32)).collect()])
    }
}

/// Map a source grid onto a surface. Grid targets map (row, col)→row*cols+col
/// with honest cropping (a 6×22 effect on a smaller board paints what fits);
/// linear targets fill index-by-index. Unpainted cells stay `None` so lower
/// layers show through per-LED. Bounds-guarded so a mis-declared SurfaceInfo
/// (leds < rows*cols) degrades instead of panicking.
fn grid_to_cells(grid: &[Vec<u32>], surface: &SurfaceInfo) -> Vec<Option<Rgb>> {
    let mut cells = vec![None; surface.leds];
    match surface.grid {
        Some(g) => {
            for (r, row) in grid.iter().enumerate().take(g.rows) {
                for (c, v) in row.iter().enumerate().take(g.cols) {
                    if let Some(cell) = cells.get_mut(r * g.cols + c) {
                        *cell = Some(bgr(*v));
                    }
                }
            }
        }
        None => {
            for (i, v) in grid.iter().flatten().enumerate().take(surface.leds) {
                cells[i] = Some(bgr(*v));
            }
        }
    }
    cells
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Kernel;

    fn kernel() -> Kernel {
        let mut k = Kernel::new();
        k.declare(SurfaceInfo::grid("kbd", "Board", SurfaceKind::Keyboard, 6, 22));
        k
    }

    fn req(method: &str, path: &str, body: serde_json::Value) -> HttpRequest {
        HttpRequest { method: method.into(), path: path.into(), body: body.to_string().into_bytes() }
    }

    fn open_session(srv: &mut ChromaServer, k: &mut Kernel, now: Instant) -> u64 {
        let r = srv.handle(
            &req("POST", "/razer/chromasdk", serde_json::json!({ "title": "Test Game" })),
            k,
            now,
        );
        assert_eq!(r.status, 200);
        let v: serde_json::Value = serde_json::from_str(&r.body).unwrap();
        v["sessionid"].as_u64().expect("session id")
    }

    fn put_static(srv: &mut ChromaServer, k: &mut Kernel, id: u64, color: u32, now: Instant) {
        let r = srv.handle(
            &req(
                "PUT",
                &format!("/razer/chromasdk/sess/{id}/keyboard"),
                serde_json::json!({ "effect": "CHROMA_STATIC", "param": { "color": color } }),
            ),
            k,
            now,
        );
        assert_eq!(r.status, 200);
    }

    #[test]
    fn sessions_roster_is_ttl_honest() {
        let mut k = kernel();
        let now = Instant::now();
        let mut srv = ChromaServer::new();
        assert!(srv.sessions(now).is_empty());
        let id = open_session(&mut srv, &mut k, now);
        let live = srv.sessions(now);
        assert_eq!(live.len(), 1);
        assert_eq!(live[0].1, "Test Game", "the roster carries the game's title");
        // A heartbeat-lapsed session stops being REPORTED at the moment its
        // lease would lapse — before any request-driven prune actually runs.
        assert!(
            srv.sessions(now + SESSION_TTL + Duration::from_secs(1)).is_empty(),
            "a vanished game must age out of the roster with its lease"
        );
        // And an explicit uninit empties it immediately.
        let r = srv.handle(
            &req("DELETE", &format!("/razer/chromasdk/sess/{id}"), serde_json::json!({})),
            &mut k,
            now,
        );
        assert_eq!(r.status, 200);
        assert!(srv.sessions(now).is_empty());
    }

    #[test]
    fn init_returns_port_plausible_session_and_routable_uri() {
        let mut k = kernel();
        let mut srv = ChromaServer::new();
        let r = srv.handle(
            &req("POST", "/razer/chromasdk", serde_json::json!({ "title": "Test Game" })),
            &mut k,
            Instant::now(),
        );
        let v: serde_json::Value = serde_json::from_str(&r.body).unwrap();
        let id = v["sessionid"].as_u64().unwrap();
        assert!(id >= 54236, "session ids double as ports in official client flows: {id}");
        assert!(v["uri"].as_str().unwrap().contains(&format!("/sess/{id}")));
    }

    #[test]
    fn static_effect_decodes_bgr_correctly() {
        let mut k = kernel();
        let mut srv = ChromaServer::new();
        let now = Instant::now();
        let id = open_session(&mut srv, &mut k, now);
        // 0x00FF0000 in COLORREF/BGR is BLUE, not red — the classic mixup.
        put_static(&mut srv, &mut k, id, 0x00FF0000, now);
        let frame = k.resolve("kbd", now).unwrap();
        assert!(frame.iter().all(|c| *c == Some(Rgb(0, 0, 255))), "0x00FF0000 = pure blue");
    }

    #[test]
    fn heartbeat_recovers_paint_after_kernel_rebirth() {
        let mut k = kernel();
        let mut srv = ChromaServer::new();
        let now = Instant::now();
        let id = open_session(&mut srv, &mut k, now);
        put_static(&mut srv, &mut k, id, 0x00FF0000, now); // pure blue
        assert!(k.resolve("kbd", now).unwrap().iter().all(|c| *c == Some(Rgb(0, 0, 255))));
        let owner = srv.sessions(now)[0].0;

        // Kernel rebirth: a fresh kernel, surface re-declared, every lease swept
        // (leases are never reborn). The Chroma session is app-side state — it
        // survives with its owner id and retained content.
        let mut reborn = kernel();
        assert!(
            reborn.resolve("kbd", now).unwrap().iter().all(|c| c.is_none()),
            "reborn kernel starts dark"
        );

        // A game that only HEARTBEATS a static effect (no fresh writes) must still
        // recover — the old path left it dark until the next effect WRITE.
        let r = srv.handle(
            &req("PUT", &format!("/razer/chromasdk/sess/{id}/heartbeat"), serde_json::json!({})),
            &mut reborn,
            now,
        );
        assert_eq!(r.status, 200);
        let frame = reborn.resolve("kbd", now).unwrap();
        assert!(
            frame.iter().all(|c| *c == Some(Rgb(0, 0, 255))),
            "heartbeat re-claimed the static effect after rebirth"
        );
        assert_eq!(reborn.label_of(owner), Some("Test Game"), "label restored too");
    }

    #[test]
    fn custom_grid_lands_row_major() {
        let mut k = kernel();
        let mut srv = ChromaServer::new();
        let now = Instant::now();
        let id = open_session(&mut srv, &mut k, now);
        let mut grid = vec![vec![0u32; 22]; 6];
        grid[1][2] = 0x0000FF;
        srv.handle(
            &req(
                "PUT",
                &format!("/razer/chromasdk/sess/{id}/keyboard"),
                serde_json::json!({ "effect": "CHROMA_CUSTOM", "param": grid }),
            ),
            &mut k,
            now,
        );
        let frame = k.resolve("kbd", now).unwrap();
        assert_eq!(frame[22 + 2], Some(Rgb(255, 0, 0)), "row 1 col 2 = index 24");
        assert_eq!(frame[0], Some(Rgb(0, 0, 0)), "zeros paint black, not transparent");
    }

    #[test]
    fn keyboard_custom2_object_param_is_accepted() {
        // The audit's top finding: keyboard CUSTOM2 param is an OBJECT
        // {color: 8x24, key: 6x22}, not a flat array. A spec-correct payload
        // must not 400.
        let mut k = kernel();
        let mut srv = ChromaServer::new();
        let now = Instant::now();
        let id = open_session(&mut srv, &mut k, now);
        let mut color = vec![vec![0u32; 24]; 8];
        color[0][0] = 0x0000FF; // red at (0,0)
        let key = vec![vec![0u32; 22]; 6];
        let r = srv.handle(
            &req(
                "PUT",
                &format!("/razer/chromasdk/sess/{id}/keyboard"),
                serde_json::json!({ "effect": "CHROMA_CUSTOM2", "param": { "color": color, "key": key } }),
            ),
            &mut k,
            now,
        );
        let v: serde_json::Value = serde_json::from_str(&r.body).unwrap();
        assert_eq!(v["result"].as_i64(), Some(rz::SUCCESS), "object-shaped CUSTOM2 must work");
        // 8x24 source cropped onto the 6x22 surface: (0,0) survives.
        assert_eq!(k.resolve("kbd", now).unwrap()[0], Some(Rgb(255, 0, 0)));
    }

    #[test]
    fn batch_effects_apply_in_sequence_with_results() {
        let mut k = kernel();
        let mut srv = ChromaServer::new();
        let now = Instant::now();
        let id = open_session(&mut srv, &mut k, now);
        let r = srv.handle(
            &req(
                "PUT",
                &format!("/razer/chromasdk/sess/{id}/keyboard"),
                serde_json::json!({ "effects": [
                    { "effect": "CHROMA_STATIC", "param": { "color": 0x0000FF } },
                    { "effect": "CHROMA_STATIC", "param": { "color": 0xFF0000 } },
                ] }),
            ),
            &mut k,
            now,
        );
        let v: serde_json::Value = serde_json::from_str(&r.body).unwrap();
        assert_eq!(v["results"].as_array().map(|a| a.len()), Some(2));
        // Applied in sequence: the last one is showing.
        assert_eq!(k.resolve("kbd", now).unwrap()[0], Some(Rgb(0, 0, 255)));
    }

    #[test]
    fn post_stores_without_applying_and_put_effect_applies() {
        let mut k = kernel();
        let mut srv = ChromaServer::new();
        let now = Instant::now();
        let id = open_session(&mut srv, &mut k, now);
        // POST: create the effect. Nothing paints yet — a preloading game
        // must not flash every variant it creates.
        let r = srv.handle(
            &req(
                "POST",
                &format!("/razer/chromasdk/sess/{id}/keyboard"),
                serde_json::json!({ "effect": "CHROMA_STATIC", "param": { "color": 255 } }),
            ),
            &mut k,
            now,
        );
        let v: serde_json::Value = serde_json::from_str(&r.body).unwrap();
        let eid = v["id"].as_str().expect("created effect id").to_string();
        assert!(k.resolve("kbd", now).unwrap()[0].is_none(), "POST must not apply");
        // PUT …/effect with the id applies it.
        let r = srv.handle(
            &req(
                "PUT",
                &format!("/razer/chromasdk/sess/{id}/effect"),
                serde_json::json!({ "id": eid }),
            ),
            &mut k,
            now,
        );
        let v: serde_json::Value = serde_json::from_str(&r.body).unwrap();
        assert_eq!(v["result"].as_i64(), Some(rz::SUCCESS));
        assert_eq!(k.resolve("kbd", now).unwrap()[0], Some(Rgb(255, 0, 0)));
        // DELETE …/effect frees it; a second apply now reports NOT_FOUND.
        let r = srv.handle(
            &req(
                "DELETE",
                &format!("/razer/chromasdk/sess/{id}/effect"),
                serde_json::json!({ "id": eid }),
            ),
            &mut k,
            now,
        );
        let v: serde_json::Value = serde_json::from_str(&r.body).unwrap();
        assert_eq!(v["result"].as_i64(), Some(rz::SUCCESS));
        let r = srv.handle(
            &req(
                "PUT",
                &format!("/razer/chromasdk/sess/{id}/effect"),
                serde_json::json!({ "id": eid }),
            ),
            &mut k,
            now,
        );
        let v: serde_json::Value = serde_json::from_str(&r.body).unwrap();
        assert_eq!(v["result"].as_i64(), Some(rz::NOT_FOUND));
    }

    #[test]
    fn heartbeats_keep_the_session_alive_past_the_ttl() {
        let mut k = kernel();
        let mut srv = ChromaServer::new();
        let t0 = Instant::now();
        let id = open_session(&mut srv, &mut k, t0);
        put_static(&mut srv, &mut k, id, 255, t0);
        let mut t = t0;
        for _ in 0..4 {
            t += Duration::from_secs(10);
            let r = srv.handle(
                &req("PUT", &format!("/razer/chromasdk/sess/{id}/heartbeat"), serde_json::json!({})),
                &mut k,
                t,
            );
            assert_eq!(r.status, 200);
        }
        assert_eq!(k.resolve("kbd", t).unwrap()[0], Some(Rgb(255, 0, 0)), "alive at t+40s");
    }

    #[test]
    fn a_dead_game_stops_painting_and_its_session_dies_at_the_ttl() {
        let mut k = kernel();
        let mut srv = ChromaServer::new();
        let t0 = Instant::now();
        let id = open_session(&mut srv, &mut k, t0);
        put_static(&mut srv, &mut k, id, 255, t0);
        assert!(k.resolve("kbd", t0 + Duration::from_secs(14)).unwrap()[0].is_some());
        // 16s of silence: the paint is gone (lease) AND the session is dead
        // (parity with the real server — the app must re-init, a late
        // heartbeat must not resurrect it).
        let t1 = t0 + Duration::from_secs(16);
        assert!(k.resolve("kbd", t1).unwrap()[0].is_none());
        let r = srv.handle(
            &req("PUT", &format!("/razer/chromasdk/sess/{id}/heartbeat"), serde_json::json!({})),
            &mut k,
            t1,
        );
        assert_eq!(r.status, 404, "a lapsed session must not heartbeat back to life");
    }

    #[test]
    fn delete_releases_immediately() {
        let mut k = kernel();
        let mut srv = ChromaServer::new();
        let now = Instant::now();
        let id = open_session(&mut srv, &mut k, now);
        put_static(&mut srv, &mut k, id, 255, now);
        let r = srv.handle(
            &req("DELETE", &format!("/razer/chromasdk/sess/{id}"), serde_json::json!({})),
            &mut k,
            now,
        );
        assert_eq!(r.status, 200);
        assert!(k.resolve("kbd", now).unwrap()[0].is_none());
    }

    #[test]
    fn two_games_the_later_session_wins_and_uninit_reveals_the_first() {
        let mut k = kernel();
        let mut srv = ChromaServer::new();
        let now = Instant::now();
        let a = open_session(&mut srv, &mut k, now);
        let b = open_session(&mut srv, &mut k, now);
        put_static(&mut srv, &mut k, a, 0x0000FF, now); // red
        put_static(&mut srv, &mut k, b, 0xFF0000, now); // blue
        assert_eq!(k.resolve("kbd", now).unwrap()[0], Some(Rgb(0, 0, 255)), "later claim wins");
        srv.handle(
            &req("DELETE", &format!("/razer/chromasdk/sess/{b}"), serde_json::json!({})),
            &mut k,
            now,
        );
        assert_eq!(k.resolve("kbd", now).unwrap()[0], Some(Rgb(255, 0, 0)), "first shows through");
    }

    #[test]
    fn malformed_json_is_refused_and_state_untouched() {
        let mut k = kernel();
        let mut srv = ChromaServer::new();
        let now = Instant::now();
        let id = open_session(&mut srv, &mut k, now);
        let r = srv.handle(
            &HttpRequest {
                method: "PUT".into(),
                path: format!("/razer/chromasdk/sess/{id}/keyboard"),
                body: b"{not json".to_vec(),
            },
            &mut k,
            now,
        );
        assert_eq!(r.status, 400);
        assert!(k.resolve("kbd", now).unwrap()[0].is_none());
    }

    #[test]
    fn missing_device_kind_answers_with_device_not_available() {
        let mut k = kernel(); // keyboard only — no mouse declared
        let mut srv = ChromaServer::new();
        let now = Instant::now();
        let id = open_session(&mut srv, &mut k, now);
        let r = srv.handle(
            &req(
                "PUT",
                &format!("/razer/chromasdk/sess/{id}/mouse"),
                serde_json::json!({ "effect": "CHROMA_STATIC", "param": { "color": 255 } }),
            ),
            &mut k,
            now,
        );
        let v: serde_json::Value = serde_json::from_str(&r.body).unwrap();
        assert_eq!(
            v["result"].as_i64(),
            Some(rz::DEVICE_NOT_AVAILABLE),
            "RZRESULT_DEVICE_NOT_AVAILABLE (4319) is the SDK's code for this"
        );
    }

    #[test]
    fn chroma_none_clears_only_that_device_layer() {
        let mut k = kernel();
        let mut srv = ChromaServer::new();
        let now = Instant::now();
        let id = open_session(&mut srv, &mut k, now);
        put_static(&mut srv, &mut k, id, 255, now);
        let r = srv.handle(
            &req(
                "PUT",
                &format!("/razer/chromasdk/sess/{id}/keyboard"),
                serde_json::json!({ "effect": "CHROMA_NONE" }),
            ),
            &mut k,
            now,
        );
        assert_eq!(r.status, 200);
        assert!(k.resolve("kbd", now).unwrap()[0].is_none());
    }

    #[test]
    fn session_info_get_answers_while_alive() {
        let mut k = kernel();
        let mut srv = ChromaServer::new();
        let now = Instant::now();
        let id = open_session(&mut srv, &mut k, now);
        let r = srv.handle(
            &req("GET", &format!("/razer/chromasdk/sess/{id}"), serde_json::json!({})),
            &mut k,
            now,
        );
        assert_eq!(r.status, 200);
        let r = srv.handle(
            &req("GET", "/razer/chromasdk/sess/99999", serde_json::json!({})),
            &mut k,
            now,
        );
        assert_eq!(r.status, 404);
    }

    #[test]
    fn unprefixed_chromasdk_root_is_accepted() {
        let mut k = kernel();
        let mut srv = ChromaServer::new();
        let r = srv.handle(
            &req("GET", "/chromasdk", serde_json::json!({})),
            &mut k,
            Instant::now(),
        );
        assert_eq!(r.status, 200);
    }

    #[test]
    fn firmware_effect_names_are_refused_not_faked() {
        let mut k = kernel();
        let mut srv = ChromaServer::new();
        let now = Instant::now();
        let id = open_session(&mut srv, &mut k, now);
        let r = srv.handle(
            &req(
                "PUT",
                &format!("/razer/chromasdk/sess/{id}/keyboard"),
                serde_json::json!({ "effect": "CHROMA_WAVE", "param": {} }),
            ),
            &mut k,
            now,
        );
        assert_eq!(r.status, 400);
    }
}
