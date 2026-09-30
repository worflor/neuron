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

/// How long a queued command stays valid.
pub const MAX_AGE: Duration = Duration::from_secs(30);

/// What the CLI can ask a running app to do.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "kebab-case")]
pub enum Command {
    /// Re-read every config file the app mirrors (binds, cast, profiles, routes, feel, badges).
    Reload,
    /// Apply a saved profile through the app's own device session, as the profile sheet does.
    ApplyProfile { name: String },
}

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
}
