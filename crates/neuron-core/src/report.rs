// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

//! Bug reports: a folder a person reads and attaches to an issue, with the issue already drafted.
//!
//! [`build`] writes `report.md` (laid out like `.github/ISSUE_TEMPLATE/bug_report.md`), the tail of
//! the crash log and the flight recorder into `reports/bug-<date>-<time>/` under a root, packs them
//! into one ZIP beside it, and returns a pre-filled new-issue link and a prompt for the person's
//! own coding agent. Nothing is sent anywhere: the person opens the link and attaches the ZIP. Every
//! text that leaves the machine this way passes [`scrub`] first.

use anyhow::{Context, Result};
use std::io::Write as _;
use std::path::{Path, PathBuf};

/// Where new issues are filed.
pub const NEW_ISSUE: &str = "https://github.com/worflor/neuron/issues/new";
/// The investigation skill a coding agent follows, when no copy ships beside the app.
pub const SKILL_URL: &str = "https://github.com/worflor/neuron/tree/main/skills/neuron-lazy-update";
/// The most of the crash log a report keeps: its newest lines.
const CRASH_TAIL_LINES: usize = 400;
/// A browser and GitHub both cut long links; the drafted body stays under this.
const URL_BODY_MAX: usize = 5500;

/// What goes into a report.
#[derive(Clone, Debug, Default)]
pub struct Input {
    /// The person's own words; may be empty.
    pub what: String,
    pub version: String,
    /// Connected devices, one line each (name, connection, PID).
    pub devices: Vec<String>,
    /// The whole crash log, when there is one; only its tail is kept.
    pub crash_log: Option<String>,
    /// The flight recorder's current dump.
    pub flight: Option<String>,
}

/// A report on disk and what to do with it.
#[derive(Clone, Debug)]
pub struct Report {
    pub dir: PathBuf,
    pub zip: PathBuf,
    pub issue_url: String,
    pub agent_prompt: String,
}

/// The recognised devices connected now, one line each (name, PID, mode). Enumeration only: it
/// never opens a device for writing.
#[must_use]
pub fn connected_devices(reg: &crate::registry::Registry) -> Vec<String> {
    let mut lines: Vec<String> = crate::transport::enumerate().unwrap_or_default().iter()
        .filter_map(|info| {
            let def = reg.find_for_pipe(info)?;
            Some(format!("{} · pid {:04x} · {} mode", def.name, info.pid, def.mode_for(info.pid).map_or("?", |m| m.name.as_str())))
        })
        .collect();
    lines.sort();
    lines.dedup();
    lines
}

/// The investigation skill for a coding agent: the copy installed beside the app, else the repo's.
#[must_use]
pub fn skill_location() -> String {
    std::env::current_exe().ok()
        .and_then(|exe| exe.parent().map(|dir| dir.join("skills").join("neuron-lazy-update").join("SKILL.md")))
        .filter(|path| path.exists())
        .map_or_else(|| SKILL_URL.to_string(), |path| path.display().to_string())
}

/// What identifies this person or machine in a text, to be replaced before it is shared.
#[derive(Clone, Debug, Default)]
pub struct Private {
    pub home: Option<String>,
    pub user: Option<String>,
    pub machine: Option<String>,
}

impl Private {
    /// This machine's home folder, user name and computer name.
    #[must_use]
    pub fn current() -> Private {
        let var = |k: &str| std::env::var(k).ok().filter(|v| v.len() >= 2);
        Private {
            home: var("USERPROFILE").or_else(|| var("HOME")),
            user: var("USERNAME").or_else(|| var("USER")),
            machine: var("COMPUTERNAME").or_else(|| var("HOSTNAME")),
        }
    }
}

/// `text` with the home folder, user name and computer name replaced (any case, either slash).
#[must_use]
pub fn scrub(text: &str, private: &Private) -> String {
    let mut out = text.to_string();
    let mut replace = |needle: &str, with: &str| {
        if needle.is_empty() {
            return;
        }
        for variant in [needle.to_string(), needle.replace('\\', "/")] {
            // ASCII folding keeps byte offsets: a name with other letters matches exactly
            let lower = out.to_ascii_lowercase();
            let find = variant.to_ascii_lowercase();
            let mut result = String::with_capacity(out.len());
            let mut at = 0;
            while let Some(found) = lower[at..].find(&find) {
                let start = at + found;
                result.push_str(&out[at..start]);
                result.push_str(with);
                at = start + find.len();
            }
            result.push_str(&out[at..]);
            out = result;
        }
    };
    // longest first: a computer name often contains the user name
    if let Some(home) = &private.home {
        replace(home, "~");
    }
    if let Some(machine) = &private.machine {
        replace(machine, "<pc>");
    }
    if let Some(user) = &private.user {
        replace(user, "<user>");
    }
    out
}

/// `yyyy-mm-dd` and `hhmmss` (UTC) for seconds since the Unix epoch.
fn stamp(secs: u64) -> (String, String) {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    // days since 1970-01-01 to a civil date (Howard Hinnant's algorithm)
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    (format!("{y:04}-{m:02}-{d:02}"), format!("{:02}{:02}{:02}", rem / 3600, rem % 3600 / 60, rem % 60))
}

/// Percent-encode for a URL query value.
fn encode(text: &str) -> String {
    let mut out = String::with_capacity(text.len() * 3 / 2);
    for b in text.bytes() {
        if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn os_line() -> String {
    let arch = std::env::consts::ARCH;
    match crate::compat::os_version() {
        Some([major, minor, build]) => format!("{} {major}.{minor} build {build} ({arch})", std::env::consts::OS),
        None => format!("{} ({arch})", std::env::consts::OS),
    }
}

fn tail(text: &str, lines: usize) -> String {
    let all: Vec<&str> = text.lines().collect();
    all[all.len().saturating_sub(lines)..].join("\n")
}

/// The issue body, in the bug template's sections.
fn body(input: &Input, files: &str) -> String {
    let what = if input.what.trim().is_empty() { "_(not written yet)_".to_string() } else { input.what.trim().to_string() };
    let devices = if input.devices.is_empty() { "none connected".to_string() } else { input.devices.join("\n") };
    format!(
        "**device + connection**\n{devices}\n\n**OS version**\n{}\n\n**neuron version**\n{}\n\n**what happened**\n{what}\n\n\
         **what you expected instead**\n\n\n**how to reproduce it**\n\n\n**diagnostics**\n{files}\n",
        os_line(), input.version,
    )
}

/// Write a report for `input` under `root/reports`, scrubbed of `private`, and pack it.
pub fn build(input: &Input, root: &Path, private: &Private, now_secs: u64, skill: &str) -> Result<Report> {
    let (date, time) = stamp(now_secs);
    let name = format!("bug-{date}-{time}");
    let reports = root.join("reports");
    let dir = reports.join(&name);
    std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    let mut files: Vec<(&str, String)> = Vec::new();
    if let Some(log) = input.crash_log.as_deref().filter(|l| !l.trim().is_empty()) {
        files.push(("crash-log.txt", scrub(&tail(log, CRASH_TAIL_LINES), private)));
    }
    if let Some(flight) = input.flight.as_deref().filter(|f| !f.trim().is_empty()) {
        files.push(("flight-recorder.txt", scrub(flight, private)));
    }
    let listed = if files.is_empty() {
        "no crash log or flight recorder output on this machine.".to_string()
    } else {
        format!("attached in `{name}.zip`: {}", files.iter().map(|(f, _)| format!("`{f}`")).collect::<Vec<_>>().join(", "))
    };
    let report = scrub(&body(input, &listed), private);
    files.insert(0, ("report.md", report.clone()));
    for (file, text) in &files {
        std::fs::write(dir.join(file), text).with_context(|| format!("write {file}"))?;
    }
    let zip = reports.join(format!("{name}.zip"));
    let mut archive = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let options = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
    for (file, text) in &files {
        archive.start_file(*file, options)?;
        archive.write_all(text.as_bytes())?;
    }
    std::fs::write(&zip, archive.finish()?.into_inner()).with_context(|| format!("write {}", zip.display()))?;

    let mut url_body = report;
    if url_body.len() > URL_BODY_MAX {
        let mut cut = URL_BODY_MAX;
        while !url_body.is_char_boundary(cut) {
            cut -= 1;
        }
        url_body.truncate(cut);
        url_body.push_str("\n\n_(cut short; the full report is in the attached zip)_\n");
    }
    url_body.push_str(&format!("\n_drag `{name}.zip` here to attach it._\n"));
    let title = input.what.trim().lines().next().unwrap_or("").chars().take(80).collect::<String>();
    let issue_url = format!(
        "{NEW_ISSUE}?template=bug_report.md&labels=bug,needs-triage&title={}&body={}",
        encode(&scrub(&title, private)),
        encode(&url_body),
    );
    let folder = scrub(&dir.display().to_string(), private);
    let agent_prompt = format!(
        "Help me investigate a neuron bug. The report is in {folder} (report.md, plus crash-log.txt and \
         flight-recorder.txt when present). Follow the neuron-lazy-update skill ({skill}), especially its \
         issues.md: read the report and logs, find what went wrong or how to reproduce it, check whether a \
         newer neuron fixes it, and draft a GitHub issue with what you found. Ask me before posting \
         anything or changing my setup.",
    );
    Ok(Report { dir, zip, issue_url, agent_prompt })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn private() -> Private {
        Private { home: Some(r"C:\Users\Ada".into()), user: Some("Ada".into()), machine: Some("ADA-PC".into()) }
    }

    #[test]
    fn scrub_hides_home_user_and_machine_in_any_case_and_slash() {
        let text = r"panic at C:\Users\Ada\AppData\x.rs, also c:/users/ada/y and user ada on ada-pc";
        let out = scrub(text, &private());
        assert!(!out.to_lowercase().contains("ada"), "{out}");
        assert!(out.contains(r"~\AppData\x.rs") && out.contains("~/y") && out.contains("<pc>"), "{out}");
    }

    #[test]
    fn stamp_formats_civil_dates() {
        assert_eq!(stamp(0), ("1970-01-01".into(), "000000".into()));
        assert_eq!(stamp(1_763_038_800), ("2025-11-13".into(), "130000".into()));
        assert_eq!(stamp(951_782_400), ("2000-02-29".into(), "000000".into()));
    }

    #[test]
    fn a_report_is_written_packed_scrubbed_and_drafted() {
        let root = std::env::temp_dir().join(format!("neuron-report-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let input = Input {
            what: "lighting froze after sleep\nthen it came back".into(),
            version: "0.1.8".into(),
            devices: vec!["Razer Naga V2 Pro · dongle · 00a8".into()],
            crash_log: Some((0..1000).map(|i| format!("line {i} at C:\\Users\\Ada\\x")).collect::<Vec<_>>().join("\n")),
            flight: Some("── flight recorder ── uptime 3.0s on ADA-PC".into()),
        };
        let report = build(&input, &root, &private(), 1_763_038_800, SKILL_URL).unwrap();
        assert!(report.dir.ends_with("bug-2025-11-13-130000"));
        let md = std::fs::read_to_string(report.dir.join("report.md")).unwrap();
        assert!(md.contains("lighting froze after sleep") && md.contains("Naga V2 Pro") && md.contains("0.1.8"));
        let crash = std::fs::read_to_string(report.dir.join("crash-log.txt")).unwrap();
        assert_eq!(crash.lines().count(), CRASH_TAIL_LINES, "only the newest lines are kept");
        assert!(crash.starts_with("line 600") && !crash.to_lowercase().contains("ada"));
        let flight = std::fs::read_to_string(report.dir.join("flight-recorder.txt")).unwrap();
        assert!(flight.contains("<pc>"));
        let zip = zip::ZipArchive::new(std::fs::File::open(&report.zip).unwrap()).unwrap();
        let names: Vec<&str> = zip.file_names().collect();
        assert!(names.contains(&"report.md") && names.contains(&"crash-log.txt") && names.contains(&"flight-recorder.txt"));
        assert!(report.issue_url.starts_with(NEW_ISSUE) && report.issue_url.contains("title=lighting%20froze%20after%20sleep&"));
        assert!(!report.issue_url.to_lowercase().contains("ada"));
        assert!(report.agent_prompt.contains("neuron-lazy-update") && report.agent_prompt.contains("bug-2025-11-13-130000"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_long_report_is_cut_for_the_link_but_whole_in_the_folder() {
        let root = std::env::temp_dir().join(format!("neuron-report-long-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let input = Input { what: "é".repeat(5000), version: "0.1.8".into(), ..Input::default() };
        let report = build(&input, &root, &Private::default(), 0, SKILL_URL).unwrap();
        assert!(report.issue_url.len() < 3 * URL_BODY_MAX + 2000, "the link stays bounded");
        assert!(std::fs::read_to_string(report.dir.join("report.md")).unwrap().contains(&"é".repeat(5000)));
        let _ = std::fs::remove_dir_all(&root);
    }
}
