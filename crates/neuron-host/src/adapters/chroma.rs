//! Razer Chroma SDK REST server adapter — the flagship port.
//!
//! Chroma-enabled games open a session against `localhost:54235`, heartbeat it
//! every ~1s, and PUT lighting effects per device. Crucially, the native
//! `RzChromaSDK64.dll` is itself just a client of this same REST server — so
//! one server catches BOTH native-SDK and REST games, with no DLL hijacking
//! and no anti-cheat exposure (R&D doc §4.1).
//!
//! This module is a PURE request→response state machine: no sockets, no
//! threads, no clock reads — the HTTP pump lives elsewhere and time arrives
//! as a parameter. Session teardown needs no timers at all: every session
//! layer is claimed with a 15-second TTL lease, and every effect write or
//! heartbeat refreshes it. A game that crashes simply stops refreshing, its
//! lease lapses, and the user's base lighting returns — the protocol's own
//! 15s-inactivity contract, enforced by the arbiter instead of by cleanup
//! code (§5.1: teardown is the default path).
//!
//! ## Verified vs unverified wire details
//! Verified by the research pass (Razer REST portal docs, RazerApi.md,
//! python-chroma-rest-server):
//! - endpoints: `POST /razer/chromasdk` (init), `PUT …/heartbeat`,
//!   `PUT/POST …/{device}` effects, `DELETE …` (uninit); ~1s heartbeats,
//!   15s inactivity timeout;
//! - effect names `CHROMA_NONE/STATIC/CUSTOM/CUSTOM_KEY/CUSTOM2`; keyboard
//!   grid 6×22 (CUSTOM) / 8×24 (CUSTOM2); colors are COLORREF-style BGR ints
//!   (`0x00BBGGRR`: R = v&0xFF, G = v>>8, B = v>>16);
//! - responses carry `{"result": <code>}` with 0 = success (RZRESULT reuses
//!   Windows error codes: 87 invalid parameter, 1168 not found).
//!
//! UNVERIFIED against a live RzSDKServer (flagged for the capture/replay
//! harness): the exact session-URI base and the exact init/heartbeat reply
//! field set. Both are safe here because we mint the URIs we later parse, and
//! we reply with a superset (`sessionid` + `session` + `uri`; `result` +
//! `tick`) so clients reading either field are served.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use crate::api::{HostApi, LeaseSpec, SurfaceInfo, SurfaceKind};
use crate::arbiter::{band, Content, LayerId, Rgb, SourceId};
use crate::bus::Value;

/// The Chroma SDK's own session contract.
pub const SESSION_TTL: Duration = Duration::from_secs(15);
/// Bookkeeping prune horizon (belt-and-braces beside the lease: the arbiter
/// already stopped painting at 15s; this just drops our session map entry).
const PRUNE_AFTER: Duration = Duration::from_secs(30);

/// RZRESULT codes (Windows error codes, as the SDK reuses them).
mod rz {
    pub const SUCCESS: i64 = 0;
    pub const INVALID_PARAMETER: i64 = 87;
    pub const NOT_FOUND: i64 = 1168;
}

pub struct HttpRequest {
    /// "GET" | "POST" | "PUT" | "DELETE" (uppercase).
    pub method: String,
    /// Path only, e.g. "/razer/chromasdk/sess/3/keyboard".
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
}

struct Session {
    owner: SourceId,
    /// device endpoint ("keyboard", …) → its live layer.
    layers: HashMap<String, LayerId>,
    last_seen: Instant,
    title: String,
    heartbeats: u64,
}

/// The Chroma REST server state machine. One instance serves all sessions.
pub struct ChromaServer {
    sessions: HashMap<u64, Session>,
    next_id: u64,
}

impl ChromaServer {
    pub fn new() -> ChromaServer {
        ChromaServer { sessions: HashMap::new(), next_id: 1 }
    }

    pub fn handle(
        &mut self,
        req: &HttpRequest,
        host: &mut dyn HostApi,
        now: Instant,
    ) -> HttpResponse {
        self.prune(host, now);
        let segs: Vec<&str> = req.path.split('/').filter(|s| !s.is_empty()).collect();
        // Root: ["razer", "chromasdk"].
        let root = segs.len() == 2 && segs[0] == "razer" && segs[1] == "chromasdk";
        if root {
            return match req.method.as_str() {
                "GET" => HttpResponse::json(
                    200,
                    serde_json::json!({
                        "result": rz::SUCCESS,
                        "version": env!("CARGO_PKG_VERSION"),
                        "core": "neuron-host",
                    }),
                ),
                "POST" => self.init(req, host, now),
                _ => HttpResponse::result(rz::INVALID_PARAMETER),
            };
        }
        // Session ops: any path containing ".../sess/{id}[/{rest}]".
        if let Some(pos) = segs.iter().position(|s| *s == "sess") {
            let Some(id) = segs.get(pos + 1).and_then(|s| s.parse::<u64>().ok()) else {
                return HttpResponse::json(404, serde_json::json!({ "result": rz::NOT_FOUND }));
            };
            let rest = &segs[pos + 2..];
            return match (req.method.as_str(), rest) {
                ("DELETE", []) => self.uninit(id, host),
                ("PUT", ["heartbeat"]) => self.heartbeat(id, host, now),
                ("PUT" | "POST", [device]) => self.effect(id, device, &req.body, host, now),
                _ => HttpResponse::json(404, serde_json::json!({ "result": rz::NOT_FOUND })),
            };
        }
        HttpResponse::json(404, serde_json::json!({ "result": rz::NOT_FOUND }))
    }

    /// `POST /razer/chromasdk` — open a session for an app.
    fn init(
        &mut self,
        req: &HttpRequest,
        host: &mut dyn HostApi,
        now: Instant,
    ) -> HttpResponse {
        let Ok(info) = serde_json::from_slice::<serde_json::Value>(&req.body) else {
            return HttpResponse::json(400, serde_json::json!({ "result": rz::INVALID_PARAMETER }));
        };
        let title = info
            .get("title")
            .and_then(|t| t.as_str())
            .unwrap_or("unnamed chroma app")
            .to_string();
        let id = self.next_id;
        self.next_id += 1;
        let owner = host.next_source();
        self.sessions.insert(
            id,
            Session { owner, layers: HashMap::new(), last_seen: now, title: title.clone(), heartbeats: 0 },
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

    fn uninit(&mut self, id: u64, host: &mut dyn HostApi) -> HttpResponse {
        match self.sessions.remove(&id) {
            Some(s) => {
                host.release_owner(s.owner);
                host.publish("host.chroma.closed", Value::Text(s.title));
                HttpResponse::result(rz::SUCCESS)
            }
            None => HttpResponse::json(404, serde_json::json!({ "result": rz::NOT_FOUND })),
        }
    }

    fn heartbeat(&mut self, id: u64, host: &mut dyn HostApi, now: Instant) -> HttpResponse {
        let Some(s) = self.sessions.get_mut(&id) else {
            return HttpResponse::json(404, serde_json::json!({ "result": rz::NOT_FOUND }));
        };
        s.last_seen = now;
        s.heartbeats += 1;
        for layer in s.layers.values() {
            // A swept layer here is fine: the next effect write re-claims.
            let _ = host.refresh(*layer, now);
        }
        HttpResponse::json(
            200,
            serde_json::json!({ "result": rz::SUCCESS, "tick": s.heartbeats }),
        )
    }

    /// `PUT/POST …/sess/{id}/{device}` — apply an effect.
    fn effect(
        &mut self,
        id: u64,
        device: &str,
        body: &[u8],
        host: &mut dyn HostApi,
        now: Instant,
    ) -> HttpResponse {
        let Some(kind) = device_kind(device) else {
            return HttpResponse::json(404, serde_json::json!({ "result": rz::NOT_FOUND }));
        };
        if !self.sessions.contains_key(&id) {
            return HttpResponse::json(404, serde_json::json!({ "result": rz::NOT_FOUND }));
        }
        let Ok(v) = serde_json::from_slice::<serde_json::Value>(body) else {
            return HttpResponse::json(400, serde_json::json!({ "result": rz::INVALID_PARAMETER }));
        };
        let Some(effect) = v.get("effect").and_then(|e| e.as_str()) else {
            return HttpResponse::json(400, serde_json::json!({ "result": rz::INVALID_PARAMETER }));
        };

        // Truthful capability answer: no surface of this kind → NOT_FOUND,
        // not a silent success (§5.4).
        let Some(surface) = host.surfaces().into_iter().find(|s| s.kind == kind) else {
            return HttpResponse::json(200, serde_json::json!({ "result": rz::NOT_FOUND }));
        };

        let content = match effect {
            "CHROMA_NONE" => {
                let s = self.sessions.get_mut(&id).expect("checked above");
                s.last_seen = now;
                if let Some(layer) = s.layers.remove(device) {
                    host.release(layer);
                }
                return HttpResponse::result(rz::SUCCESS);
            }
            "CHROMA_STATIC" => {
                let Some(color) =
                    v.pointer("/param/color").and_then(|c| c.as_u64()).map(|c| bgr(c as u32))
                else {
                    return HttpResponse::json(
                        400,
                        serde_json::json!({ "result": rz::INVALID_PARAMETER }),
                    );
                };
                Content::Fill(color)
            }
            "CHROMA_CUSTOM" | "CHROMA_CUSTOM2" => {
                let Some(grid) = v.get("param").and_then(parse_grid) else {
                    return HttpResponse::json(
                        400,
                        serde_json::json!({ "result": rz::INVALID_PARAMETER }),
                    );
                };
                Content::Cells(grid_to_cells(&grid, &surface))
            }
            "CHROMA_CUSTOM_KEY" => {
                // The "key" grid carries 0x01000000-flagged key codes for
                // key-translation; we honor the color grid now and note the
                // key-code remap as a later refinement.
                let Some(grid) = v.pointer("/param/color").and_then(parse_grid) else {
                    return HttpResponse::json(
                        400,
                        serde_json::json!({ "result": rz::INVALID_PARAMETER }),
                    );
                };
                Content::Cells(grid_to_cells(&grid, &surface))
            }
            _ => {
                // Firmware-side effect names (WAVE/BREATHING/…) are the
                // device's business; a REST server that pretends to run them
                // would be lying. Refuse honestly.
                return HttpResponse::json(
                    400,
                    serde_json::json!({ "result": rz::INVALID_PARAMETER }),
                );
            }
        };

        let s = self.sessions.get_mut(&id).expect("checked above");
        s.last_seen = now;
        // set-or-claim; a swept layer (game stalled >15s then resumed) is
        // re-claimed transparently.
        if let Some(layer) = s.layers.get(device) {
            if host.set_content(*layer, content.clone(), now) {
                return HttpResponse::result(rz::SUCCESS);
            }
        }
        match host.claim(&surface.key, s.owner, band::SESSION, LeaseSpec::Ttl(SESSION_TTL), content, now)
        {
            Some(layer) => {
                s.layers.insert(device.to_string(), layer);
                HttpResponse::result(rz::SUCCESS)
            }
            None => HttpResponse::json(200, serde_json::json!({ "result": rz::NOT_FOUND })),
        }
    }

    /// Drop bookkeeping for sessions silent past the prune horizon. Their
    /// layers already stopped painting at 15s (lease); this releases the
    /// owner and our map entry so a dead game doesn't accumulate state.
    fn prune(&mut self, host: &mut dyn HostApi, now: Instant) {
        let dead: Vec<u64> = self
            .sessions
            .iter()
            .filter(|(_, s)| now.duration_since(s.last_seen) > PRUNE_AFTER)
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

/// COLORREF-style BGR int → Rgb (R = v&0xFF, G = v>>8, B = v>>16).
fn bgr(v: u32) -> Rgb {
    Rgb((v & 0xFF) as u8, ((v >> 8) & 0xFF) as u8, ((v >> 16) & 0xFF) as u8)
}

/// Effect param → rows of BGR ints. Accepts a grid (array of arrays — the
/// keyboard shape) or a flat array (linear devices).
fn parse_grid(v: &serde_json::Value) -> Option<Vec<Vec<u32>>> {
    let arr = v.as_array()?;
    if arr.is_empty() {
        return Some(Vec::new());
    }
    if arr[0].is_array() {
        arr.iter()
            .map(|row| {
                row.as_array()?.iter().map(|c| c.as_u64().map(|c| c as u32)).collect()
            })
            .collect()
    } else {
        Some(vec![arr.iter().filter_map(|c| c.as_u64().map(|c| c as u32)).collect()])
    }
}

/// Map a source grid onto a surface. Grid targets map (row, col)→row*cols+col
/// with honest cropping (a 6×22 effect on a smaller board paints what fits);
/// linear targets fill index-by-index. Unpainted cells stay `None` so lower
/// layers show through per-LED.
fn grid_to_cells(grid: &[Vec<u32>], surface: &SurfaceInfo) -> Vec<Option<Rgb>> {
    let mut cells = vec![None; surface.leds];
    match surface.grid {
        Some(g) => {
            for (r, row) in grid.iter().enumerate().take(g.rows) {
                for (c, v) in row.iter().enumerate().take(g.cols) {
                    cells[r * g.cols + c] = Some(bgr(*v));
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

    fn open_session(
        srv: &mut ChromaServer,
        k: &mut Kernel,
        now: Instant,
    ) -> (u64, String) {
        let r = srv.handle(
            &req("POST", "/razer/chromasdk", serde_json::json!({ "title": "Test Game" })),
            k,
            now,
        );
        assert_eq!(r.status, 200);
        let v: serde_json::Value = serde_json::from_str(&r.body).unwrap();
        let id = v["sessionid"].as_u64().expect("session id");
        let uri = v["uri"].as_str().expect("uri").to_string();
        (id, uri)
    }

    #[test]
    fn init_returns_a_parseable_session() {
        let mut k = kernel();
        let mut srv = ChromaServer::new();
        let (id, uri) = open_session(&mut srv, &mut k, Instant::now());
        assert!(uri.contains(&format!("/sess/{id}")), "uri {uri} must route back to the session");
    }

    #[test]
    fn static_effect_decodes_bgr_correctly() {
        let mut k = kernel();
        let mut srv = ChromaServer::new();
        let now = Instant::now();
        let (id, _) = open_session(&mut srv, &mut k, now);
        // 0x00FF0000 in COLORREF/BGR is BLUE, not red — the classic mixup the
        // adapter must get right.
        let r = srv.handle(
            &req(
                "PUT",
                &format!("/razer/chromasdk/sess/{id}/keyboard"),
                serde_json::json!({ "effect": "CHROMA_STATIC", "param": { "color": 0x00FF0000u32 } }),
            ),
            &mut k,
            now,
        );
        assert_eq!(r.status, 200);
        let frame = k.resolve("kbd", now).unwrap();
        assert!(frame.iter().all(|c| *c == Some(Rgb(0, 0, 255))), "0x00FF0000 = pure blue");
    }

    #[test]
    fn custom_grid_lands_row_major() {
        let mut k = kernel();
        let mut srv = ChromaServer::new();
        let now = Instant::now();
        let (id, _) = open_session(&mut srv, &mut k, now);
        // A 6×22 grid, all zero except row 1 col 2 = 0x0000FF (red).
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
    fn heartbeats_keep_the_session_alive_past_the_ttl() {
        let mut k = kernel();
        let mut srv = ChromaServer::new();
        let t0 = Instant::now();
        let (id, _) = open_session(&mut srv, &mut k, t0);
        srv.handle(
            &req(
                "PUT",
                &format!("/razer/chromasdk/sess/{id}/keyboard"),
                serde_json::json!({ "effect": "CHROMA_STATIC", "param": { "color": 255 } }),
            ),
            &mut k,
            t0,
        );
        // Heartbeat every 10s out to t0+40s.
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
    fn a_dead_game_stops_painting_at_the_ttl() {
        let mut k = kernel();
        let mut srv = ChromaServer::new();
        let t0 = Instant::now();
        let (id, _) = open_session(&mut srv, &mut k, t0);
        srv.handle(
            &req(
                "PUT",
                &format!("/razer/chromasdk/sess/{id}/keyboard"),
                serde_json::json!({ "effect": "CHROMA_STATIC", "param": { "color": 255 } }),
            ),
            &mut k,
            t0,
        );
        assert!(k.resolve("kbd", t0 + Duration::from_secs(14)).unwrap()[0].is_some());
        // No heartbeat, no goodbye — 16s later the paint is GONE, with no
        // cleanup code having run anywhere. Teardown is the default path.
        assert!(k.resolve("kbd", t0 + Duration::from_secs(16)).unwrap()[0].is_none());
    }

    #[test]
    fn delete_releases_immediately() {
        let mut k = kernel();
        let mut srv = ChromaServer::new();
        let now = Instant::now();
        let (id, _) = open_session(&mut srv, &mut k, now);
        srv.handle(
            &req(
                "PUT",
                &format!("/razer/chromasdk/sess/{id}/keyboard"),
                serde_json::json!({ "effect": "CHROMA_STATIC", "param": { "color": 255 } }),
            ),
            &mut k,
            now,
        );
        let r = srv.handle(&req("DELETE", &format!("/razer/chromasdk/sess/{id}"), serde_json::json!({})), &mut k, now);
        assert_eq!(r.status, 200);
        assert!(k.resolve("kbd", now).unwrap()[0].is_none());
        // The session is gone: further effects 404.
        let r = srv.handle(
            &req(
                "PUT",
                &format!("/razer/chromasdk/sess/{id}/keyboard"),
                serde_json::json!({ "effect": "CHROMA_STATIC", "param": { "color": 255 } }),
            ),
            &mut k,
            now,
        );
        assert_eq!(r.status, 404);
    }

    #[test]
    fn two_games_the_later_session_wins_and_uninit_reveals_the_first() {
        let mut k = kernel();
        let mut srv = ChromaServer::new();
        let now = Instant::now();
        let (a, _) = open_session(&mut srv, &mut k, now);
        let (b, _) = open_session(&mut srv, &mut k, now);
        let paint = |srv: &mut ChromaServer, k: &mut Kernel, id: u64, color: u32| {
            srv.handle(
                &req(
                    "PUT",
                    &format!("/razer/chromasdk/sess/{id}/keyboard"),
                    serde_json::json!({ "effect": "CHROMA_STATIC", "param": { "color": color } }),
                ),
                k,
                now,
            );
        };
        paint(&mut srv, &mut k, a, 0x0000FF); // red
        paint(&mut srv, &mut k, b, 0xFF0000); // blue
        assert_eq!(k.resolve("kbd", now).unwrap()[0], Some(Rgb(0, 0, 255)), "later claim wins");
        srv.handle(&req("DELETE", &format!("/razer/chromasdk/sess/{b}"), serde_json::json!({})), &mut k, now);
        assert_eq!(k.resolve("kbd", now).unwrap()[0], Some(Rgb(255, 0, 0)), "first shows through");
    }

    #[test]
    fn malformed_json_is_refused_and_state_untouched() {
        let mut k = kernel();
        let mut srv = ChromaServer::new();
        let now = Instant::now();
        let (id, _) = open_session(&mut srv, &mut k, now);
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
    fn missing_device_kind_answers_honestly() {
        let mut k = kernel(); // keyboard only — no mouse declared
        let mut srv = ChromaServer::new();
        let now = Instant::now();
        let (id, _) = open_session(&mut srv, &mut k, now);
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
        assert_eq!(v["result"].as_i64(), Some(rz::NOT_FOUND), "no mouse → say so, don't fake it");
    }

    #[test]
    fn chroma_none_clears_only_that_device_layer() {
        let mut k = kernel();
        let mut srv = ChromaServer::new();
        let now = Instant::now();
        let (id, _) = open_session(&mut srv, &mut k, now);
        srv.handle(
            &req(
                "PUT",
                &format!("/razer/chromasdk/sess/{id}/keyboard"),
                serde_json::json!({ "effect": "CHROMA_STATIC", "param": { "color": 255 } }),
            ),
            &mut k,
            now,
        );
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
    fn silent_sessions_are_pruned_from_bookkeeping() {
        let mut k = kernel();
        let mut srv = ChromaServer::new();
        let t0 = Instant::now();
        let (id, _) = open_session(&mut srv, &mut k, t0);
        // 31s of silence, then ANY request prunes it; the old id now 404s.
        let t1 = t0 + Duration::from_secs(31);
        let r = srv.handle(
            &req("PUT", &format!("/razer/chromasdk/sess/{id}/heartbeat"), serde_json::json!({})),
            &mut k,
            t1,
        );
        assert_eq!(r.status, 404, "pruned session must not heartbeat back to life");
    }

    #[test]
    fn firmware_effect_names_are_refused_not_faked() {
        let mut k = kernel();
        let mut srv = ChromaServer::new();
        let now = Instant::now();
        let (id, _) = open_session(&mut srv, &mut k, now);
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
