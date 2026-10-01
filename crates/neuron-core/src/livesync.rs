// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

//! Hand-off from a CLI edit to a running neuron app.
//!
//! The CLI writes config files; the resident app holds its own copy in memory. A CLI process
//! appends a command to `live.queue` in the run root and the app, which polls for that file on its
//! UI tick, claims it by renaming it away and acts on each line. The app deleting the file is the
//! acknowledgement, so the CLI can tell whether an app picked its edit up. Entries older than
//! [`MAX_AGE`] are dropped, so commands sent while no app ran can never fire at a later launch.
//! No sockets, no named kernel objects: the same mechanism on every platform.

use serde::{Deserialize, Serialize};
use std::io::Write as _;
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use std::sync::atomic::{AtomicU64, Ordering};

/// How long a queued command stays valid.
pub const MAX_AGE: Duration = Duration::from_secs(30);
const POCKET_REQUEST_TTL: Duration = Duration::from_secs(8);
const MAX_POCKET_REPLY: u64 = 22 * 1024 * 1024 + 512 * 1024;
const MAX_POCKET_BYTES: usize = 16 * 1024 * 1024;
const MAX_POCKET_FORMATS: usize = 4096;
const MAX_REQUEST_FILE: u64 = 4096;
const MAX_REQUEST_FILES_TO_PRUNE: usize = 256;
static REQUEST_NONCE: AtomicU64 = AtomicU64::new(1);

/// What the CLI can ask a running app to do.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "kebab-case")]
pub enum Command {
    /// Re-read every config file the app mirrors (binds, cast, profiles, routes, feel, badges).
    Reload,
    /// Apply a saved profile through the app's own device session, as the profile sheet does.
    ApplyProfile { name: String },
    /// Handle one bounded pocket-management request stored beneath the run root.
    PocketRequest { request_id: String },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "kebab-case")]
pub enum PocketRequest {
    List,
    Inspect { slot: String },
    Delete { slot: String },
    History,
    HistoryItem { index: usize },
    ClearHistory,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "result", rename_all = "kebab-case")]
pub enum PocketReply {
    Pockets { entries: Vec<PocketMetadata> },
    Pocket { slot: String, durable: bool, contents: Option<WirePocket> },
    Deleted { removed: bool },
    History { entries: Vec<HistoryMetadata> },
    HistoryItem { contents: Option<WirePocket> },
    Cleared,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PocketMetadata { pub slot: String, pub durable: bool, pub kind: String, pub formats: Vec<u32>, pub bytes: usize }

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryMetadata { pub index: usize, pub kind: String, pub formats: Vec<u32>, pub bytes: usize }

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WirePocket { pub formats: Vec<WireFormat> }

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WireFormat { pub id: u32, pub data_base64: String }

fn metadata_kind(formats: &[u32]) -> &'static str {
    if formats.contains(&13) || formats.contains(&1) { "text" }
    else if formats.contains(&15) { "files" }
    else if formats.contains(&8) || formats.contains(&17) { "image" }
    else if formats.is_empty() { "empty" }
    else { "other" }
}

impl WirePocket {
    fn encode(pocket: crate::pocket::Pocket) -> Result<Self, String> {
        use base64::Engine;
        let total = pocket.formats.iter().try_fold(0usize, |size, format| size.checked_add(format.bytes.len()))
            .ok_or_else(|| "pocket size overflow".to_string())?;
        if total > MAX_POCKET_BYTES { return Err("explicit pocket inspection is limited to 16 MiB".into()); }
        if pocket.formats.len() > MAX_POCKET_FORMATS { return Err("explicit pocket inspection is limited to 4096 formats".into()); }
        Ok(Self { formats: pocket.formats.into_iter().map(|format| WireFormat {
            id: format.id,
            data_base64: base64::engine::general_purpose::STANDARD.encode(format.bytes),
        }).collect() })
    }

    pub fn decode(self) -> Result<crate::pocket::Pocket, String> {
        use base64::Engine;
        if self.formats.len() > MAX_POCKET_FORMATS { return Err("resident pocket has too many formats".into()); }
        let encoded_limit = MAX_POCKET_BYTES.saturating_add(2) / 3 * 4 + MAX_POCKET_FORMATS * 4;
        let encoded_total = self.formats.iter().try_fold(0usize, |size, format| size.checked_add(format.data_base64.len()))
            .ok_or_else(|| "resident pocket payload size overflow".to_string())?;
        if encoded_total > encoded_limit { return Err("resident pocket exceeds 16 MiB".into()); }
        let mut formats = Vec::with_capacity(self.formats.len());
        let mut total = 0usize;
        for format in self.formats {
            if format.data_base64.len() > MAX_POCKET_BYTES.saturating_add(2) / 3 * 4 { return Err("resident pocket exceeds 16 MiB".into()); }
            let bytes = base64::engine::general_purpose::STANDARD.decode(format.data_base64)
                .map_err(|_| "resident pocket payload is invalid base64".to_string())?;
            total = total.checked_add(bytes.len()).ok_or_else(|| "pocket size overflow".to_string())?;
            if total > MAX_POCKET_BYTES { return Err("resident pocket payload exceeds 16 MiB".into()); }
            formats.push(crate::pocket::ClipFormat { id: format.id, bytes });
        }
        Ok(crate::pocket::Pocket { formats })
    }
}

#[derive(Serialize, Deserialize)]
struct RequestFile { at: u64, request: PocketRequest }

#[derive(Serialize, Deserialize)]
struct ReplyFile { at: u64, reply: Result<PocketReply, String> }

#[derive(Serialize, Deserialize)]
struct Entry {
    at: u64,
    #[serde(flatten)]
    cmd: Command,
}

#[must_use]
pub fn queue_path() -> PathBuf {
    crate::runroot::run_root().join("live.queue")
}

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64)
}

fn request_dir() -> PathBuf { crate::runroot::run_root().join("pocket-requests") }

fn valid_request_id(id: &str) -> bool {
    id.len() == 32 && id.bytes().all(|b| b.is_ascii_hexdigit())
}

fn request_path(id: &str, suffix: &str) -> Result<PathBuf, String> {
    if !valid_request_id(id) { return Err("invalid pocket request id".into()); }
    Ok(request_dir().join(format!("{id}.{suffix}")))
}

fn prune_request_files() {
    let Ok(entries) = std::fs::read_dir(request_dir()) else { return; };
    for entry in entries.take(MAX_REQUEST_FILES_TO_PRUNE).flatten() {
        let path = entry.path();
        let Some(file) = path.file_name().and_then(|name| name.to_str()) else { continue; };
        let Some((id, suffix)) = file.rsplit_once('.') else { continue; };
        if !valid_request_id(id) || !matches!(suffix, "req" | "resp" | "tmp") { continue; }
        let expired = entry.metadata().ok().and_then(|meta| meta.modified().ok())
            .is_some_and(|modified| SystemTime::now().duration_since(modified).unwrap_or_default() > POCKET_REQUEST_TTL);
        if expired { let _ = std::fs::remove_file(path); }
    }
}

/// Send a bounded request to the resident app and wait briefly for its typed reply.
/// Returns `Ok(None)` when no app serves this run root.
pub fn request_pockets(request: &PocketRequest, wait: Duration) -> Result<Option<PocketReply>, String> {
    if app_on_this_root().is_none() { return Ok(None); }
    std::fs::create_dir_all(request_dir()).map_err(|e| format!("cannot create request directory: {e}"))?;
    prune_request_files();
    let nonce = REQUEST_NONCE.fetch_add(1, Ordering::Relaxed);
    let id = format!("{:016x}{:08x}{:08x}", now_ms(), std::process::id(), nonce as u32);
    let req_path = request_path(&id, "req")?;
    let request_bytes = serde_json::to_vec(&RequestFile { at: now_ms(), request: request.clone() })
        .map_err(|e| format!("cannot encode request: {e}"))?;
    if request_bytes.len() > 4096 { return Err("pocket request is too large".into()); }
    std::fs::OpenOptions::new().write(true).create_new(true).open(&req_path)
        .and_then(|mut file| { use std::io::Write; file.write_all(&request_bytes)?; file.sync_all() })
        .map_err(|e| format!("cannot create request: {e}"))?;
    if let Err(error) = send(&Command::PocketRequest { request_id: id.clone() }) {
        let _ = std::fs::remove_file(&req_path);
        return Err(format!("cannot queue request: {error}"));
    }
    let reply_path = request_path(&id, "resp")?;
    let deadline = std::time::Instant::now() + wait.min(POCKET_REQUEST_TTL);
    while std::time::Instant::now() < deadline {
        if reply_path.exists() {
            let result = read_reply(&reply_path);
            let _ = std::fs::remove_file(&reply_path);
            let _ = std::fs::remove_file(&req_path);
            return result.map(Some);
        }
        std::thread::sleep(Duration::from_millis(40));
    }
    let _ = std::fs::remove_file(&req_path);
    Err("resident app did not reply before the request expired".into())
}

fn read_reply(path: &std::path::Path) -> Result<PocketReply, String> {
    let bytes = read_limited(path, MAX_POCKET_REPLY).map_err(|e| format!("cannot read reply: {e}"))?;
    let file: ReplyFile = serde_json::from_slice(&bytes).map_err(|e| format!("invalid resident reply: {e}"))?;
    if file.at > now_ms() || now_ms().saturating_sub(file.at) > POCKET_REQUEST_TTL.as_millis() as u64 {
        return Err("resident reply expired".into());
    }
    file.reply
}

/// Resident-app side of the pocket request protocol. Stale, missing, oversized, or malformed
/// request files have no effect and never produce a reply containing clipboard bytes.
pub fn serve_pocket_request(id: &str) -> Result<(), String> {
    prune_request_files();
    let request_file = request_path(id, "req")?;
    let response_path = request_path(id, "resp")?;
    if !request_file.exists() { return Err("request missing or withdrawn".into()); }
    let bytes = read_limited(&request_file, MAX_REQUEST_FILE).map_err(|e| format!("request read failed: {e}"))?;
    let request: RequestFile = serde_json::from_slice(&bytes).map_err(|e| format!("request invalid: {e}"))?;
    if request.at > now_ms() || now_ms().saturating_sub(request.at) > POCKET_REQUEST_TTL.as_millis() as u64 {
        let _ = std::fs::remove_file(&request_file);
        return Err("request expired".into());
    }
    let reply = execute_pocket_request(request.request);
    if !request_file.exists() { return Err("request withdrawn before completion".into()); }
    if let Err(error) = write_reply(&response_path, ReplyFile { at: now_ms(), reply }) {
        let _ = std::fs::remove_file(&request_file);
        return Err(error);
    }
    if !request_file.exists() { let _ = std::fs::remove_file(response_path); return Err("request withdrawn before reply".into()); }
    let _ = std::fs::remove_file(request_file);
    Ok(())
}

fn execute_pocket_request(request: PocketRequest) -> Result<PocketReply, String> {
    use crate::pocket;
    match request {
        PocketRequest::List => Ok(PocketReply::Pockets { entries: pocket::metadata().into_iter().map(|(slot, durable, formats, bytes)| PocketMetadata { slot, durable, kind: metadata_kind(&formats).into(), formats, bytes }).collect() }),
        PocketRequest::Inspect { slot } => {
            pocket::validate_slot_name(&slot).map_err(str::to_string)?;
            let durable = pocket::metadata().into_iter().find(|(name, _, _, _)| name == &slot).is_some_and(|(_, durable, _, _)| durable);
            let contents = pocket::inspect_bounded(&slot, MAX_POCKET_BYTES).map_err(|e| e.to_string())?.map(WirePocket::encode).transpose()?;
            Ok(PocketReply::Pocket { slot: slot.clone(), durable, contents })
        }
        PocketRequest::Delete { slot } => pocket::delete(&slot).map(|removed| PocketReply::Deleted { removed }),
        PocketRequest::History => Ok(PocketReply::History { entries: pocket::history_metadata().into_iter().map(|(index, formats, bytes)| HistoryMetadata { index, kind: metadata_kind(&formats).into(), formats, bytes }).collect() }),
        PocketRequest::HistoryItem { index } => {
            let contents = pocket::history_item(index).map(WirePocket::encode).transpose()?;
            Ok(PocketReply::HistoryItem { contents })
        }
        PocketRequest::ClearHistory => { pocket::clear_history(); Ok(PocketReply::Cleared) }
    }
}

fn write_reply(path: &std::path::Path, reply: ReplyFile) -> Result<(), String> {
    let mut out = CappedVec { bytes: Vec::new(), limit: MAX_POCKET_REPLY as usize };
    serde_json::to_writer(&mut out, &reply).map_err(|e| format!("reply encoding failed: {e}"))?;
    let bytes = out.bytes;
    let temp = path.with_extension("tmp");
    let write_result = std::fs::OpenOptions::new().write(true).create_new(true).open(&temp)
        .and_then(|mut file| { use std::io::Write; file.write_all(&bytes)?; file.sync_all() });
    if let Err(error) = write_result { let _ = std::fs::remove_file(&temp); return Err(format!("reply write failed: {error}")); }
    std::fs::rename(&temp, path).map_err(|e| { let _ = std::fs::remove_file(&temp); format!("reply publish failed: {e}") })
}

fn read_limited(path: &std::path::Path, limit: u64) -> std::io::Result<Vec<u8>> {
    use std::io::Read;
    let file = std::fs::File::open(path)?;
    if file.metadata()?.len() > limit { return Err(std::io::Error::other("file exceeds the size limit")); }
    let mut bytes = Vec::new();
    file.take(limit.saturating_add(1)).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > limit { return Err(std::io::Error::other("file grew past the size limit")); }
    Ok(bytes)
}

struct CappedVec { bytes: Vec<u8>, limit: usize }

impl std::io::Write for CappedVec {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if self.bytes.len().checked_add(bytes.len()).is_none_or(|end| end > self.limit) {
            return Err(std::io::Error::other("reply exceeds the 22.5 MiB limit"));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> { Ok(()) }
}

/// Queue a command for a running app.
pub fn send(cmd: &Command) -> std::io::Result<()> {
    send_at(cmd, now_ms())
}

fn send_at(cmd: &Command, at: u64) -> std::io::Result<()> {
    let line = serde_json::to_string(&Entry { at, cmd: cmd.clone() }).map_err(std::io::Error::other)?;
    std::fs::create_dir_all(crate::runroot::run_root())?;
    let mut f = std::fs::OpenOptions::new().create(true).append(true).open(queue_path())?;
    writeln!(f, "{line}")
}

fn app_pid_path() -> PathBuf {
    crate::runroot::run_root().join("app.pid")
}

/// App side: record this process as the app serving the current run root, so a CLI on the same
/// root can tell its app from one running on another root.
pub fn publish_app_root() {
    let root = crate::runroot::run_root();
    let _ = std::fs::create_dir_all(&root);
    let _ = std::fs::write(app_pid_path(), format!("{}
{}
", std::process::id(), root.display()));
}

/// CLI side: the pid of a live app that published this run root, if there is one.
#[must_use]
pub fn app_on_this_root() -> Option<u32> {
    let text = std::fs::read_to_string(app_pid_path()).ok()?;
    let pid: u32 = text.lines().next()?.trim().parse().ok()?;
    pid_alive(pid).then_some(pid)
}

#[cfg(windows)]
fn pid_alive(pid: u32) -> bool {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};
    const STILL_ACTIVE: u32 = 259;
    // SAFETY: a failed open returns null and is not used; a valid handle is closed exactly once.
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if h.is_null() {
            return false;
        }
        let mut code = 0u32;
        let ok = GetExitCodeProcess(h, &mut code) != 0 && code == STILL_ACTIVE;
        CloseHandle(h);
        ok
    }
}

#[cfg(not(windows))]
fn pid_alive(pid: u32) -> bool {
    std::path::Path::new(&format!("/proc/{pid}")).exists()
}

/// Is a command waiting to be claimed?
#[must_use]
pub fn pending() -> bool {
    queue_path().exists()
}

/// App side: claim every queued command. The rename makes the claim atomic, so two readers can
/// never both act on one command. Malformed and stale lines are dropped.
#[must_use]
pub fn take() -> Vec<Command> {
    let path = queue_path();
    if !path.exists() {
        return Vec::new();
    }
    let claimed = path.with_extension("queue.claimed");
    if std::fs::rename(&path, &claimed).is_err() {
        return Vec::new();
    }
    let text = std::fs::read_to_string(&claimed).unwrap_or_default();
    let _ = std::fs::remove_file(&claimed);
    let cutoff = now_ms().saturating_sub(MAX_AGE.as_millis() as u64);
    text.lines()
        .filter_map(|l| serde_json::from_str::<Entry>(l).ok())
        .filter(|e| e.at >= cutoff)
        .map(|e| e.cmd)
        .collect()
}

/// CLI side: send `cmd` and wait up to `wait` for an app to claim it. `true` means a running app
/// picked it up; `false` means none did (the file is withdrawn), and the edit is on disk for the
/// next launch to read.
pub fn notify_app(cmd: &Command, wait: Duration) -> std::io::Result<bool> {
    send(cmd)?;
    let deadline = std::time::Instant::now() + wait;
    while std::time::Instant::now() < deadline {
        if !pending() {
            return Ok(true);
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let taken = !pending();
    if !taken {
        let _ = std::fs::remove_file(queue_path());
    }
    Ok(taken)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commands_round_trip_in_order_and_the_claim_empties_the_queue() {
        let _r = crate::authoring::test_run_root();
        assert!(take().is_empty());
        send(&Command::Reload).unwrap();
        send(&Command::ApplyProfile { name: "game".into() }).unwrap();
        assert!(pending());
        assert_eq!(take(), vec![Command::Reload, Command::ApplyProfile { name: "game".into() }]);
        assert!(!pending(), "claiming removes the queue: that is the acknowledgement");
        assert!(take().is_empty());
    }

    #[test]
    fn stale_and_malformed_entries_never_fire() {
        let _r = crate::authoring::test_run_root();
        send_at(&Command::Reload, 1).unwrap();
        std::fs::OpenOptions::new().append(true).open(queue_path()).and_then(|mut f| writeln!(f, "not json")).unwrap();
        send(&Command::ApplyProfile { name: "fresh".into() }).unwrap();
        assert_eq!(take(), vec![Command::ApplyProfile { name: "fresh".into() }]);
    }

    #[test]
    fn a_published_app_is_found_only_on_its_own_root() {
        let _r = crate::authoring::test_run_root();
        assert_eq!(app_on_this_root(), None);
        publish_app_root();
        assert_eq!(app_on_this_root(), Some(std::process::id()));
        std::fs::write(app_pid_path(), "4294967
x
").unwrap();
        assert_eq!(app_on_this_root(), None, "a dead pid is not an app");
    }

    #[test]
    fn notify_reports_whether_an_app_claimed_it() {
        let _r = crate::authoring::test_run_root();
        // nobody claims: withdrawn, reported false, nothing left to fire at a later launch
        assert!(!notify_app(&Command::Reload, Duration::from_millis(120)).unwrap());
        assert!(!pending());
        // an "app" that claims mid-wait
        let root = crate::runroot::run_root();
        let t = std::thread::spawn(move || {
            let deadline = std::time::Instant::now() + Duration::from_secs(2);
            while std::time::Instant::now() < deadline {
                if root.join("live.queue").exists() {
                    let _ = std::fs::remove_file(root.join("live.queue"));
                    return;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        });
        assert!(notify_app(&Command::Reload, Duration::from_secs(2)).unwrap());
        t.join().unwrap();
    }

    #[test]
    fn pocket_requests_round_trip_through_the_existing_live_queue() {
        let _r = crate::authoring::test_run_root();
        publish_app_root();
        let app = std::thread::spawn(|| {
            let deadline = std::time::Instant::now() + Duration::from_secs(3);
            while std::time::Instant::now() < deadline {
                for command in take() {
                    if let Command::PocketRequest { request_id } = command {
                        serve_pocket_request(&request_id).unwrap();
                        return;
                    }
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            panic!("pocket request was not delivered");
        });
        let reply = request_pockets(&PocketRequest::History, Duration::from_secs(2)).unwrap().unwrap();
        assert!(matches!(reply, PocketReply::History { .. }));
        app.join().unwrap();
        assert!(!pending());
    }

    #[test]
    fn request_paths_reject_non_generated_ids() {
        assert!(request_path("../../outside", "req").is_err());
        assert!(request_path("not-hex-000000000000000000000000", "resp").is_err());
    }

    #[test]
    fn stale_or_withdrawn_pocket_requests_do_not_run_and_metadata_has_no_body() {
        let _r = crate::authoring::test_run_root();
        let slot = format!("wire-secret-{}", std::process::id());
        crate::pocket::testclip::seed_slot(&slot, crate::pocket::testclip::text_pocket("clipboard-body-must-not-leak"), false);

        let stale_id = format!("{:032x}", 1u128);
        let stale_path = request_path(&stale_id, "req").unwrap();
        std::fs::create_dir_all(request_dir()).unwrap();
        std::fs::write(&stale_path, serde_json::to_vec(&RequestFile {
            at: 1,
            request: PocketRequest::Delete { slot: slot.clone() },
        }).unwrap()).unwrap();
        assert!(serve_pocket_request(&stale_id).is_err());
        assert!(crate::pocket::inspect(&slot).is_some());

        let withdrawn_id = format!("{:032x}", 2u128);
        let withdrawn_path = request_path(&withdrawn_id, "req").unwrap();
        std::fs::write(&withdrawn_path, serde_json::to_vec(&RequestFile {
            at: now_ms(),
            request: PocketRequest::Delete { slot: slot.clone() },
        }).unwrap()).unwrap();
        std::fs::remove_file(&withdrawn_path).unwrap();
        assert!(serve_pocket_request(&withdrawn_id).is_err());
        assert!(crate::pocket::inspect(&slot).is_some());

        let reply = execute_pocket_request(PocketRequest::List).unwrap();
        let json = serde_json::to_string(&reply).unwrap();
        assert!(json.contains(&slot));
        assert!(!json.contains("clipboard-body-must-not-leak"));
        let _ = crate::pocket::delete(&slot);
    }

    #[test]
    fn explicit_inspection_stops_at_the_wire_payload_limit() {
        let pocket = crate::pocket::Pocket { formats: vec![crate::pocket::ClipFormat { id: 13, bytes: vec![0; MAX_POCKET_BYTES + 1] }] };
        assert!(WirePocket::encode(pocket).unwrap_err().contains("limited to 16 MiB"));
    }
}
