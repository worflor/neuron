// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

//! GitHub Releases transport: the conditional latest-release query, the signed manifest, resumable
//! verified asset downloads and the notes media. Blocking; call it from a worker.

use super::manifest::{self, Manifest, MediaFile, MAX_MANIFEST_BYTES, MAX_MEDIA_BYTES};
use super::version::Version;
use sha2::{Digest, Sha256};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// Where releases come from and which key they must be signed with.
#[derive(Clone, Debug)]
pub struct Endpoints {
    /// `https://api.github.com`
    pub api: String,
    /// `owner/name`
    pub repo: String,
    /// `https://raw.githubusercontent.com`, for notes media at the release tag.
    pub raw: String,
    /// Asset URLs the release API reports must start with this.
    pub download_prefix: String,
    pub key: [u8; 32],
}

impl Endpoints {
    pub fn github(repo: &str, key: [u8; 32]) -> Self {
        Self {
            api: "https://api.github.com".into(),
            repo: repo.into(),
            raw: "https://raw.githubusercontent.com".into(),
            download_prefix: format!("https://github.com/{repo}/releases/download/"),
            key,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RemoteAsset {
    pub name: String,
    pub url: String,
    pub size: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct LatestRelease {
    pub tag: String,
    #[serde(default)]
    pub html_url: String,
    pub assets: Vec<RemoteAsset>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Check {
    NotModified,
    Release { release: LatestRelease, etag: Option<String> },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
    Offline,
    RateLimited { retry_at: Option<u64> },
    Http(u16),
    NoRelease,
    BadSignature,
    Malformed(String),
    HashMismatch,
    SizeMismatch,
    DiskFull,
    Io(String),
    NoAsset,
    Foreign,
    Cancelled,
}

impl Error {
    /// A few words for the header pill.
    pub fn short(&self) -> String {
        match self {
            Error::Offline => "offline".into(),
            Error::RateLimited { .. } => "rate-limited".into(),
            Error::Http(code) => format!("http {code}"),
            Error::NoRelease => "no release".into(),
            Error::BadSignature => "bad signature".into(),
            Error::Malformed(_) => "bad manifest".into(),
            Error::HashMismatch => "hash mismatch".into(),
            Error::SizeMismatch => "size mismatch".into(),
            Error::DiskFull => "disk full".into(),
            Error::Io(_) => "disk error".into(),
            Error::NoAsset => "no package".into(),
            Error::Foreign => "foreign url".into(),
            Error::Cancelled => "cancelled".into(),
        }
    }

    /// Errors that say nothing is wrong with the release, only with reaching it.
    pub fn transient(&self) -> bool {
        matches!(self, Error::Offline | Error::RateLimited { .. } | Error::Http(500..=599) | Error::Io(_))
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Malformed(m) | Error::Io(m) => write!(f, "{}: {m}", self.short()),
            other => f.write_str(&other.short()),
        }
    }
}

impl From<manifest::ManifestError> for Error {
    fn from(e: manifest::ManifestError) -> Self {
        match e {
            manifest::ManifestError::BadSignature => Error::BadSignature,
            manifest::ManifestError::Malformed(m) => Error::Malformed(m),
        }
    }
}

fn io_error(e: &std::io::Error) -> Error {
    let full = e.kind() == std::io::ErrorKind::StorageFull
        || matches!(e.raw_os_error(), Some(code) if (cfg!(windows) && (code == 112 || code == 39)) || (!cfg!(windows) && code == 28));
    if full {
        Error::DiskFull
    } else {
        Error::Io(e.to_string())
    }
}

fn net_error(e: &ureq::Error) -> Error {
    match e {
        ureq::Error::StatusCode(code) => Error::Http(*code),
        ureq::Error::BodyExceedsLimit(_) => Error::SizeMismatch,
        ureq::Error::Io(io) if io.kind() == std::io::ErrorKind::StorageFull => Error::DiskFull,
        _ => Error::Offline,
    }
}

fn agent(redirects: u32) -> ureq::Agent {
    let config = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .max_redirects(redirects)
        .timeout_connect(Some(Duration::from_secs(15)))
        .timeout_recv_response(Some(Duration::from_secs(30)))
        .timeout_recv_body(Some(Duration::from_secs(60)))
        .user_agent(concat!("neuron-updater/", env!("CARGO_PKG_VERSION")))
        .build();
    config.into()
}

fn now_unix() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

fn header<'a>(resp: &'a ureq::http::Response<ureq::Body>, name: &str) -> Option<&'a str> {
    resp.headers().get(name).and_then(|v| v.to_str().ok())
}

fn status_error(resp: &ureq::http::Response<ureq::Body>) -> Error {
    let code = resp.status().as_u16();
    let exhausted = header(resp, "x-ratelimit-remaining") == Some("0");
    let retry_after = header(resp, "retry-after").and_then(|s| s.trim().parse::<u64>().ok());
    if code == 429 || (code == 403 && (exhausted || retry_after.is_some())) {
        let reset = header(resp, "x-ratelimit-reset").and_then(|s| s.trim().parse::<u64>().ok());
        return Error::RateLimited { retry_at: retry_after.map(|s| now_unix() + s).or(reset) };
    }
    if code == 404 {
        return Error::NoRelease;
    }
    Error::Http(code)
}

#[derive(serde::Deserialize)]
struct ApiAsset {
    name: String,
    browser_download_url: String,
    size: u64,
}

#[derive(serde::Deserialize)]
struct ApiRelease {
    tag_name: String,
    #[serde(default)]
    html_url: String,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    assets: Vec<ApiAsset>,
}

/// `GET /repos/{repo}/releases/latest`, conditional on `etag`. A 304 does not count against the
/// unauthenticated rate limit.
pub fn latest(ep: &Endpoints, etag: Option<&str>) -> Result<Check, Error> {
    let url = format!("{}/repos/{}/releases/latest", ep.api.trim_end_matches('/'), ep.repo);
    let mut req = agent(3).get(&url).header("Accept", "application/vnd.github+json").header("X-GitHub-Api-Version", "2022-11-28");
    if let Some(tag) = etag {
        req = req.header("If-None-Match", tag);
    }
    let mut resp = req.call().map_err(|e| net_error(&e))?;
    match resp.status().as_u16() {
        304 => Ok(Check::NotModified),
        200 => {
            let etag = header(&resp, "etag").map(str::to_string);
            let body = resp.body_mut().with_config().limit(2 * 1024 * 1024).read_to_vec().map_err(|e| net_error(&e))?;
            let api: ApiRelease = serde_json::from_slice(&body).map_err(|e| Error::Malformed(e.to_string()))?;
            if api.draft {
                return Err(Error::NoRelease);
            }
            let release = LatestRelease {
                tag: api.tag_name,
                html_url: api.html_url,
                assets: api.assets.into_iter().map(|a| RemoteAsset { name: a.name, url: a.browser_download_url, size: a.size }).collect(),
            };
            Ok(Check::Release { release, etag })
        }
        _ => Err(status_error(&resp)),
    }
}

fn asset_url<'a>(ep: &Endpoints, release: &'a LatestRelease, name: &str) -> Result<&'a RemoteAsset, Error> {
    let asset = release.assets.iter().find(|a| a.name == name).ok_or(Error::NoAsset)?;
    if !asset.url.starts_with(&ep.download_prefix) {
        return Err(Error::Foreign);
    }
    Ok(asset)
}

fn fetch_small(url: &str, limit: u64) -> Result<Vec<u8>, Error> {
    let mut resp = agent(5).get(url).call().map_err(|e| net_error(&e))?;
    if resp.status().as_u16() != 200 {
        return Err(status_error(&resp));
    }
    resp.body_mut().with_config().limit(limit).read_to_vec().map_err(|e| net_error(&e))
}

/// A verified manifest with the exact bytes it was verified from.
#[derive(Clone, Debug)]
pub struct Signed {
    pub manifest: Manifest,
    pub version: Version,
    pub body: Vec<u8>,
    pub signature: Vec<u8>,
}

/// Fetch and verify the release's signed manifest. Refuses a manifest whose tag is not the
/// release's.
pub fn manifest(ep: &Endpoints, release: &LatestRelease) -> Result<Signed, Error> {
    let body = fetch_small(&asset_url(ep, release, manifest::MANIFEST_NAME)?.url, MAX_MANIFEST_BYTES)?;
    let signature = fetch_small(&asset_url(ep, release, manifest::SIGNATURE_NAME)?.url, 4096)?;
    let (m, v) = manifest::verify(&body, &signature, &ep.key)?;
    if m.tag != release.tag {
        return Err(Error::Malformed("manifest belongs to another release".into()));
    }
    Ok(Signed { manifest: m, version: v, body, signature })
}

/// The download URL for a manifest asset, checked against the release listing.
pub fn asset_for<'a>(ep: &Endpoints, release: &'a LatestRelease, asset: &manifest::Asset) -> Result<&'a RemoteAsset, Error> {
    let remote = asset_url(ep, release, &asset.name)?;
    if remote.size != asset.size {
        return Err(Error::SizeMismatch);
    }
    Ok(remote)
}

/// SHA-256 of a file, lowercase hex.
pub fn sha256_file(path: &Path) -> std::io::Result<String> {
    let mut f = std::fs::File::open(path)?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(hex(&h.finalize()))
}

pub fn sha256_bytes(bytes: &[u8]) -> String {
    hex(&Sha256::digest(bytes))
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes.iter().fold(String::with_capacity(bytes.len() * 2), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    })
}

/// The in-progress name beside a download's final path.
pub fn partial_path(dest: &Path) -> PathBuf {
    let mut name = dest.file_name().map(std::ffi::OsStr::to_os_string).unwrap_or_default();
    name.push(".part");
    dest.with_file_name(name)
}

/// Download `url` to `dest`, resuming a `.part` file when the server honours `Range`. The final
/// name appears only after the size and SHA-256 match; a mismatch deletes the partial file.
/// `progress(done, total)` runs as bytes arrive.
pub fn download(url: &str, dest: &Path, size: u64, sha256: &str, progress: &mut dyn FnMut(u64, u64), cancel: &AtomicBool) -> Result<(), Error> {
    if dest.is_file() {
        if sha256_file(dest).map_err(|e| io_error(&e))? == sha256 {
            progress(size, size);
            return Ok(());
        }
        std::fs::remove_file(dest).map_err(|e| io_error(&e))?;
    }
    if let Some(dir) = dest.parent() {
        std::fs::create_dir_all(dir).map_err(|e| io_error(&e))?;
    }
    let part = partial_path(dest);
    let mut have = std::fs::metadata(&part).map(|m| m.len()).unwrap_or(0);
    if have > size {
        std::fs::remove_file(&part).map_err(|e| io_error(&e))?;
        have = 0;
    }
    let result = fetch_into(url, &part, have, size, progress, cancel);
    match result {
        Ok(()) => {}
        Err(Error::HashMismatch | Error::SizeMismatch) => {
            let _ = std::fs::remove_file(&part);
            return result;
        }
        Err(e) => return Err(e),
    }
    let len = std::fs::metadata(&part).map_err(|e| io_error(&e))?.len();
    if len != size {
        let _ = std::fs::remove_file(&part);
        return Err(Error::SizeMismatch);
    }
    if sha256_file(&part).map_err(|e| io_error(&e))? != sha256 {
        let _ = std::fs::remove_file(&part);
        return Err(Error::HashMismatch);
    }
    std::fs::rename(&part, dest).map_err(|e| io_error(&e))
}

fn fetch_into(url: &str, part: &Path, have: u64, size: u64, progress: &mut dyn FnMut(u64, u64), cancel: &AtomicBool) -> Result<(), Error> {
    if have == size {
        return Ok(());
    }
    let agent = agent(0);
    let mut target = url.to_string();
    let mut resp = None;
    for _ in 0..6 {
        let mut req = agent.get(&target);
        if have > 0 {
            req = req.header("Range", format!("bytes={have}-"));
        }
        let r = req.call().map_err(|e| net_error(&e))?;
        if r.status().is_redirection() {
            let Some(next) = header(&r, "location") else { return Err(Error::Http(r.status().as_u16())) };
            target = next.to_string();
            continue;
        }
        resp = Some(r);
        break;
    }
    let Some(mut resp) = resp else { return Err(Error::Http(310)) };
    let (mut file, mut done) = match resp.status().as_u16() {
        206 if have > 0 => {
            let starts_at = header(&resp, "content-range")
                .and_then(|r| r.strip_prefix("bytes "))
                .and_then(|r| r.split('-').next())
                .and_then(|s| s.parse::<u64>().ok());
            if starts_at != Some(have) {
                return Err(Error::SizeMismatch);
            }
            (std::fs::OpenOptions::new().append(true).open(part).map_err(|e| io_error(&e))?, have)
        }
        200 => (std::fs::File::create(part).map_err(|e| io_error(&e))?, 0),
        416 => return Err(Error::SizeMismatch),
        _ => return Err(status_error(&resp)),
    };
    progress(done, size);
    let mut reader = resp.body_mut().with_config().limit(size - done + 1).reader();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        if cancel.load(Ordering::Relaxed) {
            let _ = file.sync_all();
            return Err(Error::Cancelled);
        }
        let n = match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) => {
                let _ = file.sync_all();
                return Err(if e.kind() == std::io::ErrorKind::Other && e.to_string().contains("limit") { Error::SizeMismatch } else { Error::Offline });
            }
        };
        done += n as u64;
        if done > size {
            return Err(Error::SizeMismatch);
        }
        file.write_all(&buf[..n]).map_err(|e| io_error(&e))?;
        progress(done, size);
    }
    file.sync_all().map_err(|e| io_error(&e))?;
    if done < size {
        return Err(Error::Offline);
    }
    Ok(())
}

/// A notes media file at the release tag, verified against the manifest and cached by hash.
pub fn media(ep: &Endpoints, tag: &str, file: &MediaFile, cache: &Path) -> Result<Vec<u8>, Error> {
    let ext = Path::new(&file.path).extension().and_then(|e| e.to_str()).unwrap_or("bin");
    let cached = cache.join(format!("{}.{ext}", file.sha256));
    if let Ok(bytes) = std::fs::read(&cached) {
        if bytes.len() as u64 == file.size && sha256_bytes(&bytes) == file.sha256 {
            return Ok(bytes);
        }
    }
    if file.size > MAX_MEDIA_BYTES {
        return Err(Error::SizeMismatch);
    }
    let url = format!("{}/{}/{tag}/{}", ep.raw.trim_end_matches('/'), ep.repo, file.path);
    let bytes = fetch_small(&url, file.size + 1)?;
    if bytes.len() as u64 != file.size {
        return Err(Error::SizeMismatch);
    }
    if sha256_bytes(&bytes) != file.sha256 {
        return Err(Error::HashMismatch);
    }
    if std::fs::create_dir_all(cache).is_ok() {
        let _ = crate::salvage::atomic_write(&cached, &bytes);
    }
    Ok(bytes)
}
