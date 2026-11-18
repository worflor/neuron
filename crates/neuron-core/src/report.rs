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
/// A browser and GitHub both cut long links; the drafted body stays under this many characters
/// once percent-encoded, where a space or a newline is three and an accented letter six.
const URL_BODY_MAX: usize = 6000;

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
    /// The home folder's 8.3 short form (`C:\Users\ADALOV~1`), when it has one of its own.
    pub home_short: Option<String>,
    pub user: Option<String>,
    pub machine: Option<String>,
}

impl Private {
    /// This machine's home folder, user name and computer name.
    #[must_use]
    pub fn current() -> Private {
        let var = |k: &str| std::env::var(k).ok().filter(|v| v.len() >= 2);
        let home = var("USERPROFILE").or_else(|| var("HOME"));
        Private {
            home_short: home.as_deref().and_then(short_path).filter(|s| Some(s) != home.as_ref()),
            home,
            user: var("USERNAME").or_else(|| var("USER")),
            machine: var("COMPUTERNAME").or_else(|| var("HOSTNAME")),
        }
    }
}

/// The 8.3 short form of `path`, as old tools and some crash logs print it.
#[cfg(windows)]
fn short_path(path: &str) -> Option<String> {
    use windows_sys::Win32::Storage::FileSystem::GetShortPathNameW;
    let wide: Vec<u16> = path.encode_utf16().chain(std::iter::once(0)).collect();
    let mut buf = vec![0u16; 1024];
    // SAFETY: `wide` is NUL-terminated and `buf` is writable for the length passed.
    let n = unsafe { GetShortPathNameW(wide.as_ptr(), buf.as_mut_ptr(), buf.len() as u32) } as usize;
    (n > 0 && n < buf.len()).then(|| String::from_utf16_lossy(&buf[..n]))
}

#[cfg(not(windows))]
fn short_path(_: &str) -> Option<String> {
    None
}

/// `text` with the home folder, user name and computer name replaced (any case, either slash).
#[must_use]
pub fn scrub(text: &str, private: &Private) -> String {
    let mut out = text.to_string();
    // a path is replaced wherever it appears; a name only as a whole word, so a person called
    // "user" or "neuron" does not have those words cut out of the rest of the report
    let mut replace = |needle: &str, with: &str, whole: bool| {
        if needle.is_empty() {
            return;
        }
        // a path in both slash forms; a name with no backslash is one form, replaced once (twice
        // would replace inside its own "<user>")
        let mut variants = vec![needle.to_string()];
        if needle.contains('\\') {
            variants.push(needle.replace('\\', "/"));
        }
        for variant in variants {
            // ASCII folding keeps byte offsets: a name with other letters matches exactly
            let lower = out.to_ascii_lowercase();
            let find = variant.to_ascii_lowercase();
            let mut result = String::with_capacity(out.len());
            let mut at = 0;
            let word = |i: usize| lower.as_bytes().get(i).is_none_or(|b| !b.is_ascii_alphanumeric() && *b != b'_');
            let mut from = 0;
            while let Some(found) = lower[from..].find(&find) {
                let start = from + found;
                let end = start + find.len();
                if whole && !((start == 0 || word(start - 1)) && word(end)) {
                    from = start + 1;
                    while !lower.is_char_boundary(from) {
                        from += 1;
                    }
                    continue;
                }
                result.push_str(&out[at..start]);
                result.push_str(with);
                at = end;
                from = end;
            }
            result.push_str(&out[at..]);
            out = result;
        }
    };
    // longest first: a computer name often contains the user name
    // the short form first: the full home is a prefix of it ("C:\Users\Ada" of "C:\Users\ADA~1")
    if let Some(short) = &private.home_short {
        replace(short, "~", false);
    }
    if let Some(home) = &private.home {
        replace(home, "~", false);
    }
    if let Some(machine) = &private.machine {
        replace(machine, "<pc>", true);
    }
    if let Some(user) = &private.user {
        replace(user, "<user>", true);
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

/// The device report form, filled in with what neuron knows about the device: its name, how it is
/// connected (when the link mode says), and this system and version. The person adds what worked.
#[must_use]
pub fn device_report_url(name: &str, mode: &str, version: &str) -> String {
    let link = mode.to_ascii_lowercase();
    // the form's own option labels; anything unrecognised is left for the person to pick
    let connection = if link.contains("bluetooth") {
        "Bluetooth"
    } else if ["dongle", "receiver", "wireless", "hyperspeed"].iter().any(|w| link.contains(w)) {
        "USB receiver / dongle"
    } else if link.contains("wired") || link.contains("usb") {
        "Wired / USB"
    } else {
        ""
    };
    let mut url = format!(
        "{NEW_ISSUE}?template=device_report.yml&title={}&device={}&version={}",
        encode(&format!("[device] {name}")),
        encode(name),
        encode(&format!("{}, neuron v{version}", os_line())),
    );
    if !connection.is_empty() {
        url.push_str("&connection=");
        url.push_str(&encode(connection));
    }
    url
}

/// What Neuron already knows about a learned device, for the report form's optional fields.
#[derive(Clone, Debug, Default)]
pub struct DeviceEvidence {
    /// Windows Dynamic Lighting's switch, when Windows says.
    pub dynamic_lighting: Option<bool>,
    /// What the generated definition says and where each lighting value came from (see
    /// [`definition_summary`]), with the last settle's outcome.
    pub device_file: Option<String>,
    /// The lighting probe summary kept at adoption.
    pub probe: Option<String>,
}

impl DeviceEvidence {
    /// Gather it for a learned device: the definition summary and settle record, the kept probe and
    /// the Dynamic Lighting switch. Missing pieces stay `None`.
    #[must_use]
    pub fn gather(dialect_id: &str, pid: u16) -> Self {
        let file = std::fs::read_to_string(crate::synth::auto_def_path(dialect_id, pid)).ok();
        let mut device_file = file.as_deref().and_then(definition_summary);
        if let Some(r) = crate::calibrate::record_for(dialect_id, pid) {
            let settle = format!("settled: {:?}\n{}", r.verdict, r.notes.join("\n")).to_lowercase();
            device_file = Some(match device_file {
                Some(s) => format!("{s}\n{settle}"),
                None => settle,
            });
        }
        DeviceEvidence {
            dynamic_lighting: crate::dynlight::state().map(|d| d.enabled),
            device_file,
            probe: std::fs::read_to_string(crate::lightprobe::evidence_path(dialect_id, pid)).ok(),
        }
    }

    /// [`gather`](Self::gather) for the learned device named `name`, found among the generated
    /// files, so a device whose definition no longer resolves still carries its evidence.
    #[must_use]
    pub fn for_name(name: &str) -> Self {
        let dir = crate::runroot::run_root().join("devices").join("auto");
        let found = std::fs::read_dir(dir).ok().into_iter().flatten().flatten().find_map(|e| {
            let path = e.path();
            if path.extension().is_none_or(|x| x != "toml") {
                return None;
            }
            let def: crate::registry::DeviceDef = toml::from_str(&std::fs::read_to_string(&path).ok()?).ok()?;
            if def.name != name {
                return None;
            }
            Some((def.dialect.clone(), def.modes.first()?.product_id))
        });
        found.map_or_else(DeviceEvidence::default, |(dialect, pid)| DeviceEvidence::gather(&dialect, pid))
    }
}

/// A generated definition's lighting in a few lines: the bytes it sends and the provenance lines
/// the file carries (probed, atlas, hardware-verified). Short enough for an issue form, unlike the
/// whole file. `None` when `text` is not a definition.
#[must_use]
pub fn definition_summary(text: &str) -> Option<String> {
    let def: crate::registry::DeviceDef = toml::from_str(text).ok()?;
    let mut s = format!(
        "{} · tx 0x{:02X} · collection {:04X}/{:04X}{}",
        def.codename,
        def.transaction_id,
        def.control_interface.usage_page,
        def.control_interface.usage,
        def.control_interface.interface.map_or(String::new(), |i| format!(" on interface {i}"))
    );
    match &def.lighting {
        Some(l) => {
            let tx = |t: Option<u8>| t.map_or("default".to_string(), |t| format!("0x{t:02X}"));
            s.push_str(&format!(
                "\nlighting: {:?} {:?} · led 0x{:02X} · {}x{} · effects tx {} · brightness {}",
                l.protocol,
                l.framing,
                l.led_id,
                l.rows,
                l.cols,
                tx(l.effect.transaction_id),
                l.brightness.as_ref().map_or("none".into(), |b| format!("tx {}", tx(b.transaction_id))),
            ));
        }
        None => s.push_str("\nlighting: none"),
    }
    for line in text.lines().filter(|l| l.starts_with("#   - ") && (l.contains("HARDWARE-VERIFIED") || l.contains("lighting") || l.contains("atlas"))) {
        s.push('\n');
        s.push_str(line.trim_start_matches("#   - "));
    }
    Some(s)
}

/// [`device_report_url`] with `evidence` in the form's optional fields, each only while the link
/// stays within the same budget as a bug report's.
#[must_use]
pub fn device_report_url_with(name: &str, mode: &str, version: &str, evidence: &DeviceEvidence) -> String {
    let mut url = device_report_url(name, mode, version);
    if let Some(on) = evidence.dynamic_lighting {
        url.push_str("&dynamic-lighting=");
        url.push_str(if on { "On" } else { "Off" });
    }
    let mut room = URL_BODY_MAX.saturating_sub(url.len());
    for (field, text) in [("evidence", evidence.probe.as_deref()), ("device-file", evidence.device_file.as_deref())] {
        let Some(text) = text.map(str::trim).filter(|t| !t.is_empty()) else {
            continue;
        };
        let block = format!("```\n{text}\n```");
        let cost = field.len() + 2 + encoded_len(&block);
        if cost <= room {
            url.push_str(&format!("&{field}={}", encode(&block)));
            room -= cost;
        }
    }
    url
}

/// How long `text` is once [`encode`]d.
fn encoded_len(text: &str) -> usize {
    text.bytes().map(|b| if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) { 1 } else { 3 }).sum()
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
    let reports = root.join("reports");
    // a second report in the same second gets its own folder, never the first one's files
    let name = (1..)
        .map(|n| if n == 1 { format!("bug-{date}-{time}") } else { format!("bug-{date}-{time}-{n}") })
        .find(|name| !reports.join(name).exists())
        .unwrap_or_else(|| format!("bug-{date}-{time}"));
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
    if encoded_len(&url_body) > URL_BODY_MAX {
        // keep the longest start that, encoded with the note after it, still fits
        let note = "\n\n_(cut short; the full report is in the attached zip)_\n";
        let budget = URL_BODY_MAX.saturating_sub(encoded_len(note) + 300);
        let mut used = 0;
        let cut = url_body
            .char_indices()
            .find(|(_, c)| {
                used += encoded_len(c.encode_utf8(&mut [0; 4]));
                used > budget
            })
            .map_or(url_body.len(), |(i, _)| i);
        url_body.truncate(cut);
        url_body.push_str(note);
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
        Private { home: Some(r"C:\Users\Ada".into()), home_short: Some(r"C:\Users\ADA~1".into()), user: Some("Ada".into()), machine: Some("ADA-PC".into()) }
    }

    #[test]
    fn scrub_hides_home_user_and_machine_in_any_case_and_slash() {
        let text = r"panic at C:\Users\Ada\AppData\x.rs, also c:/users/ada/y and user ada on ada-pc";
        let out = scrub(text, &private());
        assert!(!out.to_lowercase().contains("ada"), "{out}");
        assert!(out.contains(r"~\AppData\x.rs") && out.contains("~/y") && out.contains("<pc>"), "{out}");
        assert_eq!(scrub(r"C:\Users\ADA~1\Temp\z", &private()), r"~\Temp\z");
        // a name is a word: inside other words it stays
        let named = Private { user: Some("user".into()), ..Private::default() };
        assert_eq!(scrub("the user is user, users stay", &named), "the <user> is <user>, users stay");
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
        assert!(report.issue_url.len() < URL_BODY_MAX + 1500, "the link stays bounded once encoded: {}", report.issue_url.len());
        assert!(std::fs::read_to_string(report.dir.join("report.md")).unwrap().contains(&"é".repeat(5000)));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_device_report_comes_filled_in() {
        let url = device_report_url("Razer BlackWidow V4 X", "wired", "0.1.8");
        assert!(url.starts_with(NEW_ISSUE) && url.contains("template=device_report.yml"));
        assert!(url.contains("device=Razer%20BlackWidow%20V4%20X") && url.contains("title=%5Bdevice%5D%20Razer%20BlackWidow%20V4%20X"));
        assert!(url.contains("connection=Wired%20%2F%20USB") && url.contains("neuron%20v0.1.8"));
        assert!(device_report_url("Razer Viper", "dongle", "0.1.8").contains("connection=USB%20receiver%20%2F%20dongle"));
        assert!(!device_report_url("Razer Viper", "?", "0.1.8").contains("connection="), "an unknown link is left to the person");
    }

    #[test]
    fn a_device_report_carries_what_neuron_already_knows_within_the_budget() {
        let evidence = DeviceEvidence {
            dynamic_lighting: Some(true),
            device_file: Some("name = \"x\"\n".repeat(2000)),
            probe: Some("if3 000C/0001: bl:3F".into()),
        };
        let url = device_report_url_with("Razer BlackWidow V3", "wired", "0.1.9", &evidence);
        assert!(url.contains("dynamic-lighting=On"));
        assert!(url.contains("evidence=%60%60%60%0Aif3%20000C%2F0001%3A%20bl%3A3F"), "{url}");
        assert!(!url.contains("device-file="), "a file too long for the link is left out, not cut");
        assert!(url.len() <= URL_BODY_MAX, "{}", url.len());
        let small = DeviceEvidence { device_file: Some("name = \"x\"".into()), ..DeviceEvidence::default() };
        assert!(device_report_url_with("x", "wired", "0.1.9", &small).contains("device-file="));
        assert_eq!(device_report_url_with("x", "?", "0.1.9", &DeviceEvidence::default()), device_report_url("x", "?", "0.1.9"));
    }
}
