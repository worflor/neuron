// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

//! The updater against a local HTTP server standing in for GitHub: the latest-release API with
//! ETags and rate limits, signed manifests, resumable asset downloads and notes media.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use neuron::update::client::{self, Endpoints, Error};
use neuron::update::manifest::{self, Asset, Manifest, MediaFile, Platform};
use neuron::update::state::State;
use neuron::update::version::Version;
use neuron::update::Engine;
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

#[derive(Default)]
struct Behaviour {
    /// Answer the API with a rate-limit refusal.
    rate_limited: bool,
    /// Send only this many body bytes of the package (with the full Content-Length), once.
    truncate_once: Option<usize>,
    /// Ignore Range headers.
    no_range: bool,
}

struct Server {
    port: u16,
    files: Arc<Mutex<HashMap<String, Vec<u8>>>>,
    behaviour: Arc<Mutex<Behaviour>>,
    log: Arc<Mutex<Vec<(String, HashMap<String, String>)>>>,
    release_json: Arc<Mutex<String>>,
}

impl Server {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let s = Server {
            port,
            files: Arc::default(),
            behaviour: Arc::default(),
            log: Arc::default(),
            release_json: Arc::default(),
        };
        let (files, behaviour, log, release) = (s.files.clone(), s.behaviour.clone(), s.log.clone(), s.release_json.clone());
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let (files, behaviour, log, release) = (files.clone(), behaviour.clone(), log.clone(), release.clone());
                std::thread::spawn(move || serve(stream, &files, &behaviour, &log, &release));
            }
        });
        s
    }

    fn base(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    fn put(&self, path: &str, body: Vec<u8>) {
        self.files.lock().unwrap().insert(path.to_string(), body);
    }

    fn hits(&self, path: &str) -> Vec<HashMap<String, String>> {
        self.log.lock().unwrap().iter().filter(|(p, _)| p == path).map(|(_, h)| h.clone()).collect()
    }
}

fn serve(
    mut stream: TcpStream,
    files: &Mutex<HashMap<String, Vec<u8>>>,
    behaviour: &Mutex<Behaviour>,
    log: &Mutex<Vec<(String, HashMap<String, String>)>>,
    release: &Mutex<String>,
) {
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut line = String::new();
    if reader.read_line(&mut line).is_err() {
        return;
    }
    let path = line.split_whitespace().nth(1).unwrap_or("/").to_string();
    let mut headers = HashMap::new();
    loop {
        let mut h = String::new();
        if reader.read_line(&mut h).is_err() || h.trim().is_empty() {
            break;
        }
        if let Some((k, v)) = h.split_once(':') {
            headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
        }
    }
    log.lock().unwrap().push((path.clone(), headers.clone()));
    let respond = |stream: &mut TcpStream, status: &str, extra: &str, body: &[u8], send: usize| {
        let head = format!("HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n{extra}\r\n", body.len());
        let _ = stream.write_all(head.as_bytes());
        let _ = stream.write_all(&body[..send.min(body.len())]);
        let _ = stream.flush();
    };
    if path.starts_with("/repos/") {
        if behaviour.lock().unwrap().rate_limited {
            return respond(&mut stream, "403 Forbidden", "x-ratelimit-remaining: 0\r\nx-ratelimit-reset: 4000000000\r\n", b"{}", 2);
        }
        if headers.get("if-none-match").map(String::as_str) == Some("\"r1\"") {
            return respond(&mut stream, "304 Not Modified", "ETag: \"r1\"\r\n", b"", 0);
        }
        let body = release.lock().unwrap().clone().into_bytes();
        return respond(&mut stream, "200 OK", "ETag: \"r1\"\r\nContent-Type: application/json\r\n", &body, usize::MAX);
    }
    let Some(body) = files.lock().unwrap().get(&path).cloned() else {
        return respond(&mut stream, "404 Not Found", "", b"", 0);
    };
    let mut b = behaviour.lock().unwrap();
    let range = headers.get("range").and_then(|r| r.strip_prefix("bytes=")).and_then(|r| r.trim_end_matches('-').parse::<usize>().ok());
    if let (Some(from), false) = (range, b.no_range) {
        let part = &body[from.min(body.len())..];
        let extra = format!("Content-Range: bytes {from}-{}/{}\r\n", body.len().saturating_sub(1), body.len());
        drop(b);
        return respond(&mut stream, "206 Partial Content", &extra, part, usize::MAX);
    }
    let send = if path.ends_with(".zip") { b.truncate_once.take().unwrap_or(usize::MAX) } else { usize::MAX };
    drop(b);
    respond(&mut stream, "200 OK", "", &body, send);
}

struct Fixture {
    server: Server,
    key: String,
    engine: Engine,
    root: PathBuf,
    package: Vec<u8>,
}

const PACKAGE: &str = "neuron-v9.0.0-windows-x86_64.zip";

fn release_json(base: &str, tag: &str, names: &[&str]) -> String {
    let assets: Vec<String> = names
        .iter()
        .map(|n| format!(r#"{{"name":"{n}","browser_download_url":"{base}/dl/{n}","size":{}}}"#, if *n == PACKAGE { 4096 } else { 1 }))
        .collect();
    format!(r#"{{"tag_name":"{tag}","html_url":"{base}/rel","draft":false,"assets":[{}]}}"#, assets.join(","))
}

fn fixture(tag: &str) -> Fixture {
    let server = Server::start();
    let (key, public) = manifest::signing::generate().unwrap();
    let root = std::env::temp_dir().join(format!("neuron-update-e2e-{}-{}", std::process::id(), server.port));
    let _ = std::fs::remove_dir_all(&root);
    let package: Vec<u8> = (0..4096u32).map(|i| (i * 7 % 251) as u8).collect();
    let media = b"\x89PNG fake media".to_vec();
    let m = Manifest {
        schema: manifest::SCHEMA,
        version: "9.0.0".into(),
        tag: "v9.0.0".into(),
        notes: "## 9.0.0 · 2026-11-20\nthe updater.\n![pill](docs/media/pill.png)\n### new\n- **updates.** one click.".into(),
        notes_url: String::new(),
        media: vec![MediaFile { path: "docs/media/pill.png".into(), size: media.len() as u64, sha256: client::sha256_bytes(&media) }],
        assets: vec![Asset { platform: "windows-x86_64-portable".into(), name: PACKAGE.into(), size: 4096, sha256: client::sha256_bytes(&package) }],
    };
    let body = serde_json::to_vec_pretty(&m).unwrap();
    let sig = manifest::signing::sign(&body, &key).unwrap();
    server.put("/dl/neuron-update.json", body);
    server.put("/dl/neuron-update.json.sig", sig.into_bytes());
    server.put(&format!("/dl/{PACKAGE}"), package.clone());
    server.put("/raw/o/n/v9.0.0/docs/media/pill.png", media);
    *server.release_json.lock().unwrap() = release_json(&server.base(), tag, &["neuron-update.json", "neuron-update.json.sig", PACKAGE]);
    let ep = Endpoints {
        api: server.base(),
        repo: "o/n".into(),
        raw: format!("{}/raw", server.base()),
        download_prefix: format!("{}/dl/", server.base()),
        key: public,
    };
    let engine = Engine { ep, run_root: root.clone(), platform: Platform::WindowsPortable, current: Version::parse("1.0.0").unwrap() };
    Fixture { server, key, engine, root, package }
}

fn no_cancel() -> AtomicBool {
    AtomicBool::new(false)
}

#[test]
fn a_signed_release_is_offered_downloaded_and_verified() {
    let f = fixture("v9.0.0");
    let mut st = State::default();
    let offer = f.engine.check(&mut st).unwrap().expect("9.0.0 is newer than 1.0.0");
    assert_eq!(offer.version.to_string(), "9.0.0");
    assert_eq!(offer.manifest.release_notes().unwrap().sections[0].items[0].title, "updates.");
    assert_eq!(st.etag.as_deref(), Some("\"r1\""));
    assert_eq!(st.failures, 0);
    let mut seen = Vec::new();
    let path = f.engine.download(&offer, &mut |d, t| seen.push((d, t)), &no_cancel()).unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), f.package);
    assert_eq!(seen.last(), Some(&(4096, 4096)));
    assert!(f.engine.package_ready(&offer));
    assert!(!client::partial_path(&path).exists());
    let media = f.engine.media(&offer);
    assert_eq!(media.len(), 1, "the notes image verifies against the signed manifest");
    let _ = std::fs::remove_dir_all(&f.root);
}

#[test]
fn not_modified_reuses_the_cached_release_and_manifest() {
    let f = fixture("v9.0.0");
    let mut st = State::default();
    f.engine.check(&mut st).unwrap().unwrap();
    let again = f.engine.check(&mut st).unwrap().expect("still offered from the cache");
    assert_eq!(again.version.to_string(), "9.0.0");
    let api = f.server.hits("/repos/o/n/releases/latest");
    assert_eq!(api.len(), 2);
    assert_eq!(api[1].get("if-none-match").map(String::as_str), Some("\"r1\""), "the second ask is conditional");
    assert_eq!(f.server.hits("/dl/neuron-update.json").len(), 1, "a 304 re-verifies the cached manifest");
    let _ = std::fs::remove_dir_all(&f.root);
}

#[test]
fn a_rate_limit_is_recorded_with_its_reset() {
    let f = fixture("v9.0.0");
    f.server.behaviour.lock().unwrap().rate_limited = true;
    let mut st = State::default();
    assert_eq!(f.engine.check(&mut st), Err(Error::RateLimited { retry_at: Some(4_000_000_000) }));
    assert_eq!(st.failures, 1);
    assert_eq!(st.retry_at, Some(4_000_000_000));
    assert!(neuron::update::policy::next_check(st.last_check, st.failures, st.retry_at, 0) >= 4_000_000_000);
    let _ = std::fs::remove_dir_all(&f.root);
}

#[test]
fn an_interrupted_download_resumes_from_its_partial_file() {
    let f = fixture("v9.0.0");
    let mut st = State::default();
    let offer = f.engine.check(&mut st).unwrap().unwrap();
    f.server.behaviour.lock().unwrap().truncate_once = Some(1500);
    let first = f.engine.download(&offer, &mut |_, _| {}, &no_cancel());
    assert!(matches!(first, Err(Error::Offline)), "{first:?}");
    let dest = f.engine.package_path(&offer).unwrap();
    assert!(!dest.exists(), "no final file until verified");
    assert_eq!(std::fs::metadata(client::partial_path(&dest)).unwrap().len(), 1500);
    let path = f.engine.download(&offer, &mut |_, _| {}, &no_cancel()).unwrap();
    assert_eq!(std::fs::read(path).unwrap(), f.package);
    let hits = f.server.hits(&format!("/dl/{PACKAGE}"));
    assert_eq!(hits.last().and_then(|h| h.get("range")).map(String::as_str), Some("bytes=1500-"));
    let _ = std::fs::remove_dir_all(&f.root);
}

#[test]
fn a_server_that_ignores_range_restarts_cleanly() {
    let f = fixture("v9.0.0");
    let mut st = State::default();
    let offer = f.engine.check(&mut st).unwrap().unwrap();
    f.server.behaviour.lock().unwrap().truncate_once = Some(1000);
    let _ = f.engine.download(&offer, &mut |_, _| {}, &no_cancel());
    f.server.behaviour.lock().unwrap().no_range = true;
    let path = f.engine.download(&offer, &mut |_, _| {}, &no_cancel()).unwrap();
    assert_eq!(std::fs::read(path).unwrap(), f.package);
    let _ = std::fs::remove_dir_all(&f.root);
}

#[test]
fn a_package_with_the_wrong_hash_is_deleted_not_kept() {
    let f = fixture("v9.0.0");
    let mut st = State::default();
    let offer = f.engine.check(&mut st).unwrap().unwrap();
    let mut evil = f.package.clone();
    evil[100] ^= 0xff;
    f.server.put(&format!("/dl/{PACKAGE}"), evil);
    assert_eq!(f.engine.download(&offer, &mut |_, _| {}, &no_cancel()), Err(Error::HashMismatch));
    let dest = f.engine.package_path(&offer).unwrap();
    assert!(!dest.exists() && !client::partial_path(&dest).exists(), "nothing runnable is left behind");
    assert!(!f.engine.package_ready(&offer));
    let _ = std::fs::remove_dir_all(&f.root);
}

#[test]
fn a_manifest_signed_by_another_key_is_refused() {
    let f = fixture("v9.0.0");
    let (other, _) = manifest::signing::generate().unwrap();
    let body = f.server.files.lock().unwrap().get("/dl/neuron-update.json").cloned().unwrap();
    f.server.put("/dl/neuron-update.json.sig", manifest::signing::sign(&body, &other).unwrap().into_bytes());
    let mut st = State::default();
    assert_eq!(f.engine.check(&mut st), Err(Error::BadSignature));
    assert_eq!(st.failures, 1);
    let _ = f.key;
    let _ = std::fs::remove_dir_all(&f.root);
}

#[test]
fn an_older_or_equal_release_is_not_offered_and_its_manifest_not_fetched() {
    for tag in ["v1.0.0", "v0.9.9", "v1.0.0-rc.1"] {
        let f = fixture(tag);
        let mut st = State::default();
        assert_eq!(f.engine.check(&mut st), Ok(None), "{tag}");
        assert!(f.server.hits("/dl/neuron-update.json").is_empty());
        let _ = std::fs::remove_dir_all(&f.root);
    }
}

#[test]
fn media_with_a_wrong_hash_is_omitted() {
    let f = fixture("v9.0.0");
    let mut st = State::default();
    let offer = f.engine.check(&mut st).unwrap().unwrap();
    f.server.put("/raw/o/n/v9.0.0/docs/media/pill.png", b"\x89PNG fake mediX".to_vec());
    assert!(f.engine.media(&offer).is_empty());
    let file = &offer.manifest.media[0];
    assert_eq!(client::media(&f.engine.ep, "v9.0.0", file, &f.root.join("m")), Err(Error::HashMismatch));
    let _ = std::fs::remove_dir_all(&f.root);
}

#[test]
fn an_asset_url_outside_the_release_prefix_is_refused() {
    let f = fixture("v9.0.0");
    let json = release_json(&f.server.base(), "v9.0.0", &["neuron-update.json", "neuron-update.json.sig"])
        .replace("]}", &format!(r#",{{"name":"{PACKAGE}","browser_download_url":"http://evil.invalid/{PACKAGE}","size":4096}}]}}"#));
    *f.server.release_json.lock().unwrap() = json;
    let mut st = State::default();
    assert_eq!(f.engine.check(&mut st), Err(Error::Foreign));
    let _ = std::fs::remove_dir_all(&f.root);
}

#[test]
fn offline_is_reported_as_offline() {
    let ep = Endpoints {
        api: "http://127.0.0.1:9".into(),
        repo: "o/n".into(),
        raw: String::new(),
        download_prefix: String::new(),
        key: [0; 32],
    };
    let e = client::latest(&ep, None).unwrap_err();
    assert_eq!(e, Error::Offline);
    assert!(e.transient());
}
