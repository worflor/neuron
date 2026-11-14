// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

//! `neuron update`: what the updater knows, a check, an install, and a preview of the update UI.
//! The running app owns its updater; these verbs read its `update/live.json` and drive it through
//! livesync, so a person, a script and an agent all go through the same signed, verified path.
//! With no app running, `status` reads the record on disk and `check` asks the release API itself.

use crate::out;
use anyhow::{bail, Result};
use clap::Subcommand;
use neuron::livesync::{self, Command, UpdateOp};
use neuron::update::{self, client, policy, state, version::Version, Engine};
use std::time::{Duration, Instant};

#[derive(Subcommand)]
pub enum UpdateCmd {
    /// This version, the newest release, and what the updater is doing
    Status,
    /// Look for a newer release now
    Check,
    /// Download the newest release, install it and restart the app
    Install,
    /// Show the update UI in a state, with this build's own notes and no release
    ///
    /// available, downloading, ready, installing, updated, rolled-back or error; `off` ends it.
    /// The pill's buttons then walk the states as a real update would. Nothing is fetched or
    /// installed, and it works in any build, a source build included.
    Preview { phase: String },
}

pub fn run(action: UpdateCmd) -> Result<()> {
    match action {
        UpdateCmd::Status => status(),
        UpdateCmd::Check => check(),
        UpdateCmd::Install => install(),
        UpdateCmd::Preview { phase } => preview(&phase),
    }
}

fn now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

fn ago(at: Option<u64>) -> String {
    match at.map(|t| now().saturating_sub(t)) {
        None => "never".into(),
        Some(s) if s < 90 => "just now".into(),
        Some(s) if s < 5400 => format!("{} min ago", s / 60),
        Some(s) if s < 172_800 => format!("{} h ago", s / 3600),
        Some(s) => format!("{} days ago", s / 86400),
    }
}

/// Why this copy does not update itself, or `None` when it does.
fn disabled() -> Option<&'static str> {
    let exe_dir = std::env::current_exe().ok().and_then(|p| p.parent().map(std::path::Path::to_path_buf)).unwrap_or_default();
    let env = policy::Environment::detect(&exe_dir, false, update::release_key().is_some());
    policy::availability(&env).err().map(policy::Disabled::describe)
}

/// The app's live view, when an app on this run root is running and has written one.
fn live() -> Option<state::Live> {
    livesync::app_on_this_root()?;
    state::Live::load(&neuron::runroot::run_root())
}

fn document(live: Option<&state::Live>, rec: &state::State) -> serde_json::Value {
    let latest = rec.release.as_ref().map(|r| r.tag.trim_start_matches('v').to_string());
    serde_json::json!({
        "version": env!("CARGO_PKG_VERSION"),
        "updates_itself": disabled().is_none(),
        "why_not": disabled(),
        "app_running": livesync::app_on_this_root().is_some(),
        "latest_release": latest,
        "last_check": rec.last_check,
        "phase": live.map(|l| l.phase.clone()),
        "offered": live.filter(|l| !l.version.is_empty() && !l.phase.is_empty()).map(|l| l.version.clone()),
        "progress": live.filter(|l| l.phase == "downloading").map(|l| l.progress),
        "error": live.filter(|l| !l.error.is_empty()).map(|l| l.error.clone()),
        "preview": live.is_some_and(|l| l.preview),
        "status": live.map(|l| l.status.clone()),
    })
}

fn line(live: Option<&state::Live>, rec: &state::State) -> String {
    let mut out = format!("neuron {}", env!("CARGO_PKG_VERSION"));
    if let Some(why) = disabled() {
        out.push_str(&format!(" · does not update itself: {why}"));
    }
    match live {
        Some(l) if l.phase == "downloading" => out.push_str(&format!("\ndownloading v{} · {:.0}%", l.version, l.progress * 100.0)),
        Some(l) if !l.phase.is_empty() => out.push_str(&format!("\n{}{} · {}", if l.preview { "preview · " } else { "" }, l.phase, if l.error.is_empty() { format!("v{}", l.version) } else { l.error.clone() })),
        Some(l) if !l.status.is_empty() => out.push_str(&format!("\n{}", l.status)),
        _ => {}
    }
    if let Some(r) = &rec.release {
        out.push_str(&format!("\nnewest release {} · checked {}", r.tag, ago(rec.last_check)));
    } else {
        out.push_str(&format!("\nchecked {}", ago(rec.last_check)));
    }
    if livesync::app_on_this_root().is_none() {
        out.push_str("\nthe app is not running");
    }
    out
}

fn status() -> Result<()> {
    let rec = state::State::load(&neuron::runroot::run_root());
    let live = live();
    out::done(document(live.as_ref(), &rec), line(live.as_ref(), &rec));
    Ok(())
}

/// Send `op` to the app and wait until its live view is newer than `sent` and done checking.
fn ask_app(op: UpdateOp, wait: Duration) -> Result<Option<state::Live>> {
    let sent = now();
    if !livesync::notify_app(&Command::Update { op }, Duration::from_millis(2500)).unwrap_or(false) {
        bail!("the running app did not pick the request up");
    }
    let root = neuron::runroot::run_root();
    let start = Instant::now();
    while start.elapsed() < wait {
        if let Some(l) = state::Live::load(&root).filter(|l| l.at >= sent && !l.checking) {
            return Ok(Some(l));
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    Ok(state::Live::load(&root))
}

fn check() -> Result<()> {
    if let Some(why) = disabled() {
        if livesync::app_on_this_root().is_none() {
            bail!("this copy does not update itself: {why}");
        }
    }
    if livesync::app_on_this_root().is_some() {
        let live = ask_app(UpdateOp::Check, Duration::from_secs(25))?;
        let rec = state::State::load(&neuron::runroot::run_root());
        out::done(document(live.as_ref(), &rec), line(live.as_ref(), &rec));
        return Ok(());
    }
    // no app: ask the release API from here, through the same signed check the app runs
    let Some(key) = update::release_key() else { bail!("this build carries no release key") };
    let exe_dir = std::env::current_exe().ok().and_then(|p| p.parent().map(std::path::Path::to_path_buf)).unwrap_or_default();
    let platform = policy::availability(&policy::Environment::detect(&exe_dir, false, true)).map_err(|d| anyhow::anyhow!(d.describe()))?;
    let root = neuron::runroot::run_root();
    let engine = Engine { ep: client::Endpoints::github(update::REPO, key), run_root: root.clone(), platform, current: Version::current() };
    let mut rec = state::State::load(&root);
    let result = engine.check(&mut rec);
    rec.save(&root)?;
    let offered = match result {
        Ok(Some(offer)) => Some(offer.version.to_string()),
        Ok(None) => None,
        Err(e) => bail!("check failed: {}", e.short()),
    };
    let mut doc = document(None, &rec);
    doc["offered"] = serde_json::json!(offered);
    let text = match &offered {
        Some(v) => format!("v{v} is available · start neuron to install it, or run `neuron update install` while it runs"),
        None => format!("neuron {} is up to date", env!("CARGO_PKG_VERSION")),
    };
    out::done(doc, text);
    Ok(())
}

fn install() -> Result<()> {
    if livesync::app_on_this_root().is_none() {
        bail!("start neuron first: the app installs its own updates, then restarts");
    }
    if let Some(why) = disabled() {
        bail!("this copy does not update itself: {why}");
    }
    let live = ask_app(UpdateOp::Install, Duration::from_secs(25))?;
    let rec = state::State::load(&neuron::runroot::run_root());
    let text = match live.as_ref().map(|l| l.phase.as_str()) {
        Some("downloading" | "ready" | "installing") => format!("installing v{} · neuron restarts when it is done", live.as_ref().map_or("", |l| l.version.as_str())),
        Some("error") => format!("the update stopped: {}", live.as_ref().map_or("", |l| l.error.as_str())),
        _ => "no newer release to install".to_string(),
    };
    out::done(document(live.as_ref(), &rec), text);
    Ok(())
}

fn preview(phase: &str) -> Result<()> {
    const PHASES: [&str; 7] = ["available", "downloading", "ready", "installing", "updated", "rolled-back", "error"];
    if livesync::app_on_this_root().is_none() {
        bail!("start neuron first: the preview shows in the app");
    }
    let op = if phase == "off" {
        UpdateOp::EndPreview
    } else if PHASES.contains(&phase) {
        UpdateOp::Preview { phase: phase.to_string() }
    } else {
        bail!("no such phase {phase}: use one of {} or off", PHASES.join(", "));
    };
    if !livesync::notify_app(&Command::Update { op }, Duration::from_millis(2500)).unwrap_or(false) {
        bail!("the running app did not pick the request up");
    }
    out::done(
        serde_json::json!({ "preview": phase }),
        if phase == "off" { "preview ended".to_string() } else { format!("previewing {phase} · open neuron to see it; its buttons walk the states. `neuron update preview off` ends it") },
    );
    Ok(())
}
