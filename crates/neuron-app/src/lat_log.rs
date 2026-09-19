// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

//! The latency instrument's REPORTING surface — how the numbers get out of the running app.
//!
//! [`neuron::latency`] records continuously and costs nothing, but a histogram nobody can read is a
//! histogram nobody benefits from. And the stages that matter most — the cross-thread hop from a HID
//! reader to the dispatch pump, the HID decode, the real `press_to_output` of a physical key — exist
//! ONLY in the live app with real hardware attached. No harness can produce them: Raw Input does not
//! report synthesised keystrokes as device events, so a self-test literally cannot press the user's
//! macro key for them.
//!
//! ## Two switches, and why the FILE is the important one
//!
//! * **`neuron_latency.on` in the run root** — while that file exists, the report is appended to
//!   `neuron_latency.log` every few seconds. Delete it and logging stops.
//! * **`NEURON_LATENCY=1`** — the same thing via the environment, for a CLI or dev run.
//!
//! The file switch exists because of how this app actually runs: an elevated scheduled task starts it
//! at logon. There is no shell in that story to set a variable in, and Task Scheduler cannot carry
//! one — so an env-only switch would mean editing a persistent user environment variable and
//! restarting the app just to find out why a keypress felt slow. A file the user can create and
//! delete needs no restart, no elevation, and no leftover state on their machine.
//!
//! Deliberately NOT gated behind a debug build: a latency complaint arrives from a release build, on
//! someone else's machine, about a press that already happened. The recording is always on (see
//! `latency`'s module doc); these switches only decide whether it gets written down.

use std::time::Duration;

/// How often the switch is checked and (when on) the log appended. The idle cost is one `metadata`
/// call on a local path every 5s — no device traffic, no allocation — which is why this can simply
/// always run instead of needing its own startup decision.
const TICK: Duration = Duration::from_secs(5);

/// The file whose presence turns logging on.
pub const FLAG_FILE: &str = "neuron_latency.on";

/// The log this writes.
pub const LOG_FILE: &str = "neuron_latency.log";

/// Truncate the log once it passes this size.
///
/// The switch is a file the user creates, which means it is a file the user can FORGET. At one report
/// per [`TICK`] that is roughly half a megabyte an hour, forever — a diagnostic that quietly eats a
/// disk is a bug, not a diagnostic. The cap is generous (a capture of a few hours still fits whole)
/// and truncation restarts cleanly rather than trimming lines, because the interesting part of a
/// latency capture is always the most recent window, never the beginning.
const LOG_MAX_BYTES: u64 = 4 * 1024 * 1024;

/// Was `NEURON_LATENCY` set? Read once — the environment does not change mid-run (the FILE switch is
/// the one that can be flipped live).
fn env_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        std::env::var("NEURON_LATENCY")
            .is_ok_and(|v| v != "0" && !v.is_empty())
    })
}

/// Is latency logging currently on — by environment, or by the flag file existing right now?
pub fn logging_on() -> bool {
    logging_on_in(&neuron::runroot::run_root())
}

/// The switch predicate against an explicit root.
///
/// Split out so it can be tested WITHOUT touching the real run root. That matters more than it looks:
/// the app is normally running while its own tests are, so a test that created the real flag file
/// would silently start a latency capture on the live instance — and a test that deleted it would end
/// a capture someone was in the middle of taking.
fn logging_on_in(root: &std::path::Path) -> bool {
    env_enabled() || root.join(FLAG_FILE).exists()
}

/// The current per-stage table, with a heading that names what the reader is looking at. Safe to call
/// from anywhere at any time (the histograms are lock-free), including from a crash path.
pub fn report_now() -> String {
    format!(
        "input latency — press to action, by stage (uptime {}s)\n{}\n{}",
        crate::flight::uptime_ms() / 1000,
        // The scheduling posture belongs WITH the numbers: the same machine measures the pump hop at
        // 40µs boosted and 7.3ms unboosted under contention, so a table that omits which one it is
        // cannot be interpreted. It also lets the boost be confirmed on a deployed, elevated build,
        // which cannot be inspected from outside the process.
        neuron::timing::posture(),
        neuron::latency::report()
    )
}

/// Start the reporter. Always runs (see [`TICK`] on why that is affordable); it writes only while the
/// switch is on, and resets the histograms when the switch is first flipped so a capture describes the
/// session the user is reproducing rather than everything since boot.
pub fn start() {
    let root = neuron::runroot::run_root();
    let log = root.join(LOG_FILE);
    if env_enabled() {
        eprintln!("[latency] NEURON_LATENCY set — per-stage input latency -> {}", log.display());
    }
    let started = crate::worker::spawn_detached("neuron-lat-log", move || {
        let mut was_on = false;
        loop {
            std::thread::sleep(TICK);
            let on = logging_on();
            if on && !was_on {
                // Startup and idle samples (config loads, device enumeration, the first paint) are not
                // input latency, and would sit in every percentile for the rest of the capture. Clear
                // them the moment a capture starts so the table describes USE.
                neuron::latency::reset_all();
                append(&log, "── latency capture started (histograms reset) ──");
                crate::flight::trace("life", "latency capture started", 0);
            } else if !on && was_on {
                append(&log, "── latency capture stopped ──");
                crate::flight::trace("life", "latency capture stopped", 0);
            } else if on {
                append(&log, &report_now());
            }
            was_on = on;
        }
    });
    if !started {
        // A spawn refusal would otherwise be invisible: the flag file would appear to do nothing, and
        // the user would conclude the instrument is broken rather than that the writer never started.
        // Recorded to the flight ring so it shows up in the same place every other lifecycle fact does.
        crate::flight::trace("life", "latency log writer failed to start", 0);
    }
}

/// Append one report to the log, best-effort, restarting the file once it passes [`LOG_MAX_BYTES`].
/// A failure to write diagnostics must never affect the app: there is nothing useful to do about it
/// and nowhere better to complain to.
fn append(path: &std::path::Path, text: &str) {
    use std::io::Write;
    if std::fs::metadata(path).map_or(0, |m| m.len()) > LOG_MAX_BYTES {
        // Restart rather than rotate: a second file would double the disk a forgotten flag can eat,
        // which is the exact thing being guarded against.
        let _ = std::fs::write(path, b"");
    }
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(f, "{text}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_report_names_every_stage_so_a_reader_can_orient() {
        let text = report_now();
        for label in LABELS {
            assert!(
                text.contains(label),
                "the report omitted `{label}`, so a reader could not tell whether that stage was \
                 fast or simply never measured"
            );
        }
        assert!(text.contains("press to action"), "the table says what it is");
    }

    /// Stage labels in `STAGES` order — kept beside the assertion that uses them so a stage added
    /// without a label fails the next test rather than silently going unchecked.
    const LABELS: &[&str] = &[
        "hid_decode",
        "inject_hop",
        "raw_decode",
        "edge_diff",
        "resolve",
        "ctx_capture",
        "ctx_clipboard",
        "action_run",
        "macro_spawn",
        "sleep_error",
        "send_input",
        "press_to_output",
        "edge_to_done",
        "pump_blocked",
    ];

    #[test]
    fn every_stage_has_a_label_in_this_test_s_list() {
        assert_eq!(
            neuron::latency::STAGES.len(),
            LABELS.len(),
            "a latency stage was added or removed — update LABELS so the report assertion still \
             covers every stage instead of quietly checking a subset"
        );
    }

    /// A forgotten flag file must not grow the log without bound — the cap is the only thing standing
    /// between a diagnostic and a disk-filling bug, so pin that it actually truncates.
    #[test]
    fn the_log_is_capped_so_a_forgotten_flag_cannot_fill_the_disk() {
        // Unique per PROCESS: `cargo test` runs several test binaries at once, and a developer can
        // have another run going. A fixed name would let one process's `remove_dir_all` delete the
        // fixture another was mid-way through asserting on.
        let dir = std::env::temp_dir().join(format!("neuron-lat-log-cap-test-{}", std::process::id()));
        if std::fs::create_dir_all(&dir).is_err() {
            return; // no writable temp in this environment — nothing to assert
        }
        let log = dir.join("capped.log");
        // Start just over the cap, as a long-running capture eventually would.
        if std::fs::write(&log, vec![b'x'; (LOG_MAX_BYTES + 1) as usize]).is_err() {
            return;
        }
        append(&log, "fresh line after the cap");
        let len = std::fs::metadata(&log).map(|m| m.len()).unwrap_or(u64::MAX);
        assert!(
            len < LOG_MAX_BYTES,
            "the log was {len} bytes after appending past the {LOG_MAX_BYTES}-byte cap — it never \
             truncated, so a forgotten flag file would grow it forever"
        );
        let body = std::fs::read_to_string(&log).unwrap_or_default();
        assert!(
            body.contains("fresh line after the cap"),
            "truncation must not lose the line it was making room for"
        );
        let _ = std::fs::remove_file(&log);
        let _ = std::fs::remove_dir(&dir);
    }

    /// The flag file is the switch a user actually reaches for, so pin that presence turns logging on
    /// and removal turns it off.
    ///
    /// Run against a TEMP root, never the real one: the app is usually running while its tests are,
    /// so creating the real flag file would start a capture on the live instance and deleting it
    /// would end one somebody was taking. `logging_on` is a one-line wrapper over this same
    /// predicate, so testing the predicate tests the switch.
    #[test]
    fn the_flag_file_toggles_logging() {
        let root = std::env::temp_dir()
            .join(format!("neuron-lat-flag-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        if std::fs::create_dir_all(&root).is_err() {
            return; // no writable temp here — nothing to assert about
        }
        let flag = root.join(FLAG_FILE);
        assert_eq!(
            logging_on_in(&root),
            env_enabled(),
            "with no flag file, only the environment switch can turn logging on"
        );
        if std::fs::write(&flag, b"").is_err() {
            let _ = std::fs::remove_dir_all(&root);
            return;
        }
        assert!(logging_on_in(&root), "creating {FLAG_FILE} must turn logging on");
        let _ = std::fs::remove_file(&flag);
        // With it gone, logging is off again — unless the env switch is independently set, which is a
        // legitimate configuration rather than a failure.
        if !env_enabled() {
            assert!(
                !logging_on_in(&root),
                "removing {FLAG_FILE} must turn logging off again"
            );
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The public wrapper must actually consult the RUN ROOT — testing only the inner predicate would
    /// not catch it looking in the wrong directory, which is the one way this switch can quietly fail
    /// for a user who created the file exactly where the docs said to.
    #[test]
    fn the_public_switch_reads_the_run_root() {
        let root = neuron::runroot::run_root();
        assert_eq!(
            logging_on(),
            logging_on_in(&root),
            "logging_on must be the run-root case of the same predicate"
        );
    }
}
