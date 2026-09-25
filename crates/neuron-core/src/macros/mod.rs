// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

//! The macro engine — the power tier of the spine.
//!
//! A Neuron macro is **Python**, run by the [`MacroHost`] — a bundled private `CPython` kept warm as a
//! sidecar process. Macros do LITERALLY ANYTHING a program can (real `CPython`: `ctypes` into raw
//! Win32, `subprocess`, sockets, files — unsandboxed). They're registered once (imports warmed) and
//! a trigger calls the already-resident function with the captured [`Context`], so dispatch is
//! warm/real-time and the input thread never spawns or imports. A crashing macro takes down only the
//! firewalled sidecar (auto-respawned), never the app that controls the user's hardware.
//!
//! This replaced an earlier tower that compiled user *Rust* to a cdylib at runtime via `rustc`,
//! hot-loaded it with `libloading`, and gated it behind a verify-then-cache fuzz pipeline. That
//! bought sub-µs native dispatch at the cost of a heavy toolchain dependency, a compile-at-trigger
//! jank, and a sandbox-vs-power fight. The Macro Host keeps the warmth without any of it.
//!
//! Two lighter script tiers remain for "run another program": [`ScriptKind::Shell`] (an inline
//! command line) and [`ScriptKind::File`] (a path to a .ps1/.py/.exe). The [`Context`] capture
//! (`context.rs`) is shared by all paths.

pub mod context;
pub mod macro_host;
pub mod node;
pub mod policy;
pub mod pyruntime;
pub mod runner;

pub use context::Context;
pub use macro_host::{macro_host, parse_document, parse_macro, BeaconEvent, DocumentParseResult, MacroHost, ParseError, ParseResult};
pub use node::{document_to_source, nodes_to_source, py_str_literal, summarize, value_to_source, MacroDocument, MacroNode, Value};
pub use policy::{mode_from_source, set_source_mode, MacroMode, RAW_DIRECTIVE};
pub use pyruntime::{ensure_runtime, Runtime};

use crate::action::{ScriptKind, ScriptRef};
use std::process::{Child, Command};

const ARM_ENV: &str = "NEURON_INPUT_ARMED";

#[cfg(windows)]
fn hide_console(cmd: &mut Command) {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    cmd.creation_flags(CREATE_NO_WINDOW);
}

#[cfg(not(windows))]
fn hide_console(_cmd: &mut Command) {}

fn scrub_arm(mut cmd: Command, no_console: bool) -> Command {
    cmd.env_remove(ARM_ENV);
    if no_console {
        hide_console(&mut cmd);
    }
    cmd
}

/// Spawn an inline shell command with Neuron's process-authority scrub applied.
///
/// On Windows the command line is handed to `cmd` VERBATIM via `raw_arg`, not through `arg`/`args`.
/// This is a correctness fix, not a style choice: `args(["/C", cmdline])` makes Rust apply MSVC
/// argument escaping to the whole command line, which turns every embedded `"` into `\"` — a form
/// `cmd.exe` does not understand (it treats the backslash literally). So any user command containing
/// quotes, the normal way to write a path with a space, failed with "The filename, directory name, or
/// volume label syntax is incorrect" and produced nothing:
///
/// ```text
///   notepad "C:\my notes\todo.txt"      →  cmd /C "notepad \"C:\my notes\todo.txt\""   ✗
///   notepad "C:\my notes\todo.txt"      →  cmd /C notepad "C:\my notes\todo.txt"       ✓ (raw_arg)
/// ```
///
/// Verbatim is also the semantically right thing here: the string IS a shell command line the user
/// authored, meant to be parsed by the shell. It grants no authority the Shell tier did not already
/// have by definition (see this module's doc on the power tiers), and the arm gate remains the thing
/// that decides whether it runs at all.
///
/// `sh -c` needs no such treatment — the Unix path passes argv straight through with no re-quoting.
pub fn spawn_shell_command(cmdline: &str) -> std::io::Result<Child> {
    #[cfg(windows)]
    let cmd = {
        use std::os::windows::process::CommandExt;
        let mut c = Command::new("cmd");
        c.arg("/C").raw_arg(cmdline);
        c
    };
    #[cfg(not(windows))]
    let cmd = {
        let mut c = Command::new("sh");
        c.args(["-c", cmdline]);
        c
    };
    scrub_arm(cmd, true).spawn()
}

/// Spawn a script/executable path with Neuron's process-authority scrub applied.
pub fn spawn_external_path(path: &std::path::Path) -> std::io::Result<Child> {
    let ext = path
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_lowercase();
    let p = path.to_string_lossy().to_string();
    let (cmd, no_console) = match ext.as_str() {
        "ps1" if cfg!(windows) => {
            let mut c = Command::new("powershell");
            c.args(["-NoProfile", "-ExecutionPolicy", "Bypass", "-File", &p]);
            (c, true)
        }
        "ps1" => {
            let mut c = Command::new("pwsh");
            c.args(["-NoProfile", "-File", &p]);
            (c, true)
        }
        "py" => {
            let mut c = Command::new(if cfg!(windows) { "python" } else { "python3" });
            c.arg(&p);
            (c, true)
        }
        _ => {
            let c = Command::new(path);
            (c, false)
        }
    };
    scrub_arm(cmd, no_console).spawn()
}

/// Resolve and run a [`ScriptRef`] — the entry point `Action::Script` delegates to (context-free
/// convenience; the spine uses [`run_script_ctx`]).
#[must_use]
pub fn run_script(script: &ScriptRef) -> String {
    run_script_ctx(script, &Context::capture())
}

/// Context-aware dispatch — the spine's path. A [`ScriptKind::Python`] macro is FIRED ASYNC against
/// the trigger-time `ctx` (never blocking the dispatch thread — the result lands in the macro log);
/// Shell/File shell out and ignore `ctx`.
#[must_use]
pub fn run_script_ctx(script: &ScriptRef, ctx: &Context) -> String {
    match script.kind {
        ScriptKind::Python => {
            if !crate::action::input_armed() {
                return format!("macro '{}' [disarmed]", script.id);
            }
            macro_host().fire_async(&script.id, ctx)
        }
        ScriptKind::Shell => run_shell(&script.id),
        ScriptKind::File => run_external(&script.id),
    }
}

/// Run a python macro by id and WAIT (bounded) for its result — the GUI "test run" + CLI path.
/// NEVER call from the input/UI thread.
#[must_use]
pub fn test_python_macro(id: &str, ctx: &Context) -> String {
    macro_host().invoke(id, ctx)
}

// ── shell-out tiers (kept from the old engine; clean and dependency-free) ───────────────────────

/// Run an inline command line through the OS interpreter (cmd on Windows, sh elsewhere) —
/// [`ScriptKind::Shell`]. Fire-and-forget; returns a one-line result. Honors the process-spawn arm
/// gate (disarmed/test/verify => report, spawn nothing) and strips armed authority from the child.
#[must_use]
pub fn run_shell(cmd: &str) -> String {
    if !crate::action::process_spawn_armed() {
        return format!("shell `{cmd}` [disarmed]");
    }
    launch(Launch::Shell(cmd.to_string()))
}

/// Run an external script/executable by path (.ps1/.py/.exe) — [`ScriptKind::File`]. Returns a
/// one-line result. Arm-gated like the inline shell-out.
#[must_use]
pub fn run_external(path: &str) -> String {
    if !crate::action::process_spawn_armed() {
        return format!("launch `{path}` [disarmed]");
    }
    launch(Launch::Path(std::path::PathBuf::from(path)))
}

/// What to start. Carried as DATA rather than a closure so [`launch`] can start it on either the
/// current thread or a worker without having to choose before it knows which.
#[derive(Clone, Debug)]
pub enum Launch {
    /// An inline command line for the OS interpreter.
    Shell(String),
    /// A script/executable path, dispatched by extension.
    Path(std::path::PathBuf),
}

impl Launch {
    fn start(&self) -> std::io::Result<std::process::Child> {
        match self {
            Launch::Shell(cmd) => spawn_shell_command(cmd),
            Launch::Path(p) => spawn_external_path(p),
        }
    }

    fn what(&self) -> String {
        match self {
            Launch::Shell(cmd) => cmd.clone(),
            Launch::Path(p) => p.to_string_lossy().to_string(),
        }
    }
}

/// Start a process for an action, keeping the LIVE INPUT PATH unblocked.
///
/// `CreateProcess` is expensive — measured at mean 5.7ms, p99 17.4ms on the dispatch thread, which
/// services every other device edge while it waits. That made a single shell-bound key the largest
/// input stall in the app: for those milliseconds no other binding, macro key or side button was
/// dispatched at all.
///
/// So when this is called while servicing a physical input edge, the spawn is handed to the macro
/// runner pool and the pump returns immediately. When it is NOT — the CLI, a GUI test button — it
/// stays synchronous, deliberately: a one-shot CLI process can exit before a detached worker ever
/// runs, and those callers can afford the milliseconds anyway. Same action, scheduled to suit who
/// asked for it (see [`crate::latency::servicing_input_edge`]).
///
/// The cost of going async is that the returned line can no longer carry the launch result, so a
/// failure is written to stderr instead of being silently swallowed.
fn launch(what: Launch) -> String {
    let label = what.what();
    if !crate::latency::servicing_input_edge() {
        return start_inline(&what, &label);
    }
    let job = what.clone();
    let job_label = label.clone();
    // NOTE: deliberately no `latency::adopt(origin)` here, unlike the macro-sequence handoff. That
    // propagation exists so a worker's first SYNTHESISED KEYSTROKE can stamp `press_to_output`, and a
    // process launch emits nothing into the OS input stream — there is no output event to stamp, so
    // adopting an origin would only create a stamp that nothing ever consumes.
    match runner::submit(move || {
        // RE-CHECK the arm gate here, immediately before spawning.
        //
        // The caller checked it too, but that check is now separated from the spawn by a queue — a
        // time-of-check/time-of-use gap that did not exist when this ran synchronously. Between the
        // two, the process-spawn gate can legitimately close: the user flips safe mode, the app
        // shuts down, a verify/fuzz run disarms. Without this, a command accepted while armed would
        // still launch afterwards, which breaks the gate's whole contract ("disarmed => spawn
        // nothing") precisely during the transitions it exists to protect.
        // RE-CHECK the arm gate here, immediately before spawning.
        //
        // The caller checked it too, but that check is now separated from the spawn by a queue — a
        // time-of-check/time-of-use gap that did not exist when this ran synchronously. Between the
        // two, the process-spawn gate can legitimately close: the user flips safe mode, the app shuts
        // down, a verify/fuzz run disarms. Without this, a command accepted while armed would still
        // launch afterwards, breaking the gate's contract ("disarmed => spawn nothing") precisely
        // during the transitions it exists to protect.
        if !crate::action::process_spawn_armed() {
            eprintln!(
                "[action] launch skipped for `{job_label}`: process spawning was disarmed while it was queued"
            );
            return;
        }
        if let Err(e) = job.start() {
            // The status line already returned, so stderr (the app's log) is where this can still be
            // seen. Losing it entirely would make a broken command look like a working one.
            eprintln!("[action] launch failed for `{job_label}`: {e}");
        }
    }) {
        runner::Submitted::Queued => format!("launching `{label}`"),
        // REFUSED and NO-POOL are different failures and deserve different answers.
        //
        // Refused means the queue is full: four launches already running and sixty-four waiting.
        // Running this one inline would put a multi-millisecond `CreateProcess` on the dispatch pump
        // at the exact moment the machine is most overloaded — stalling every other binding to serve
        // the request that was already one too many. So it is refused and SAYS so, matching the
        // runner's stated contract and how `run_sequence` treats the same outcome.
        runner::Submitted::Refused => {
            format!("`{label}` skipped · too many launches already queued")
        }
        // No pool at all is an infrastructure failure, not overload: the worker threads could not be
        // created. Falling back to a synchronous spawn costs this one press its latency, which is far
        // better than the feature simply not working.
        runner::Submitted::NoPool => start_inline(&what, &label),
    }
}

fn start_inline(what: &Launch, label: &str) -> String {
    if !crate::action::process_spawn_armed() {
        return format!("launch `{label}` [disarmed]");
    }
    match what.start() {
        Ok(_) => format!("ran `{label}`"),
        Err(e) => format!("launch failed: {e}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::action::ScriptKind;

    #[test]
    fn shell_dispatch_is_arm_gated() {
        // Tests run DISARMED (the process-spawn gate is off by default), so a shell-out must report
        // [disarmed] and spawn NOTHING.
        let cmd = if cfg!(windows) { "cd ." } else { "true" };
        assert!(
            run_shell(cmd).contains("[disarmed]"),
            "disarmed shell-out must not spawn"
        );
    }

    /// The arm gate must be checked BEFORE the scheduling decision, on both paths. Otherwise a
    /// disarmed shell-out reached from a keypress would be handed to a worker and reported as
    /// "launching" — turning the gate that makes tests and the verify-fuzzer safe into a lie.
    #[test]
    fn the_arm_gate_wins_over_the_off_thread_path() {
        let cmd = if cfg!(windows) { "cd ." } else { "true" };
        // Off the input path (as a test normally is):
        assert!(run_shell(cmd).contains("[disarmed]"));
        // AND while servicing an input edge, which is the branch that would go to the pool.
        let reported = crate::latency::with_edge(std::time::Instant::now(), || run_shell(cmd));
        assert!(
            reported.contains("[disarmed]"),
            "a disarmed shell-out was scheduled instead of refused: {reported}"
        );
    }

    /// A quoted command line must actually run. This is the regression for a long-standing silent
    /// failure: Rust's MSVC argument escaping rewrote every `"` in a user's command as `\"`, which
    /// `cmd.exe` does not understand — so a shell macro like `notepad "C:\my notes\todo.txt"` failed
    /// with a syntax error and did nothing at all, with no obvious cause.
    ///
    /// Deliberately spawns a REAL process and checks a REAL side effect: the whole bug lived in the
    /// argument encoding between this process and `cmd.exe`, so anything short of actually running it
    /// would have tested the wrong layer. The target path contains a SPACE, because a path with a
    /// space is the entire reason a user writes quotes in the first place.
    #[test]
    #[cfg(windows)]
    fn a_shell_command_containing_quotes_actually_runs() {
        let dir = std::env::temp_dir().join("neuron quoting regression");
        let _ = std::fs::remove_dir_all(&dir);
        let child = spawn_shell_command(&format!("mkdir \"{}\"", dir.display()));
        let mut child = match child {
            Ok(c) => c,
            Err(e) => panic!("cmd could not be spawned at all: {e}"),
        };
        let _ = child.wait();
        let made = dir.exists();
        let _ = std::fs::remove_dir_all(&dir);
        assert!(
            made,
            "a quoted shell command produced nothing — the command line is being re-escaped on its \
             way to cmd.exe, so every user command with a quoted path silently fails"
        );
    }

    /// The arm gate must hold ACROSS the queue, not just at submission. Moving the spawn onto a
    /// worker put a queue between the check and the spawn, so a command accepted while armed could
    /// otherwise still launch after the gate closed (safe mode toggled, shutdown, a verify run) —
    /// which is exactly when the gate matters most.
    ///
    /// Driven through the real queue: submit a launch while "armed" from the input path, and prove
    /// the worker refuses once disarmed. Tests run disarmed, so the worker's own re-check is the
    /// thing under test — if it were missing, this would spawn a real process.
    #[test]
    fn a_queued_launch_rechecks_the_arm_gate_before_spawning() {
        assert!(
            !crate::action::process_spawn_armed(),
            "tests run with process spawning disarmed; this test depends on that"
        );
        // A command that would be unmistakable if it ever ran: it creates a directory. `mkdir` is
        // used rather than a redirect (`type nul >`) on purpose — a redirect does NOT survive the
        // trip through `cmd`, so an earlier version of this test could never have created its marker
        // and passed whether or not the gate was rechecked. A test that cannot fail is worse than no
        // test, so this one uses a command proven to work by
        // `a_shell_command_containing_quotes_actually_runs`.
        let marker = std::env::temp_dir().join("neuron-toctou-must-not-exist");
        let _ = std::fs::remove_dir_all(&marker);
        let cmd = if cfg!(windows) {
            format!("mkdir \"{}\"", marker.display())
        } else {
            format!("mkdir -p '{}'", marker.display())
        };
        // Go through `launch` directly (bypassing `run_shell`'s caller-side gate) so the WORKER's
        // re-check is the only thing that can stop it — which is precisely the gap being pinned.
        let line = crate::latency::with_edge(std::time::Instant::now(), || {
            launch(Launch::Shell(cmd))
        });
        assert!(line.starts_with("launching"), "it really was queued, not run inline: {line}");
        // Give the worker ample time to pick the job up and (correctly) refuse it.
        std::thread::sleep(std::time::Duration::from_millis(600));
        assert!(
            !marker.exists(),
            "a queued launch spawned a process while DISARMED — the arm gate does not survive the \
             queue, so disarming cannot stop work already accepted"
        );
        let _ = std::fs::remove_dir_all(&marker);
    }

    /// The scheduling rule itself: called from the input path, `launch` must hand the spawn to a
    /// worker and return WITHOUT waiting — that is the whole latency fix. Verified through the status
    /// line, since the alternative (timing a real process spawn) would be flaky by nature.
    #[test]
    fn a_keypress_launch_is_handed_off_rather_than_waited_for() {
        // `launch` is only reached when armed, so drive it directly rather than moving the global gate
        // (which other tests in this binary depend on staying off).
        let what = Launch::Shell(if cfg!(windows) { "cd ." } else { "true" }.into());
        let line = crate::latency::with_edge(std::time::Instant::now(), || launch(what.clone()));
        assert!(
            line.starts_with("launching"),
            "expected an immediate hand-off from the input path, got: {line}"
        );
        // Off the input path, the fallback checks the gate again before spawning. Tests stay
        // disarmed, so it must refuse instead of starting a process.
        let line = launch(what);
        assert!(
            line.contains("[disarmed]"),
            "the inline fallback must recheck the disarmed process gate: {line}"
        );
    }

    #[test]
    fn inline_launch_fallback_rechecks_process_spawn_gate() {
        assert!(!crate::action::process_spawn_armed());
        let what = Launch::Shell(if cfg!(windows) { "cd ." } else { "true" }.into());
        let label = what.what();
        assert!(start_inline(&what, &label).contains("[disarmed]"));
    }

    #[test]
    fn run_script_ctx_disarmed_without_a_sidecar() {
        // The non-Python tiers don't touch the Macro Host — they must work (report disarmed) even with
        // no python runtime present, proving the spine can always dispatch them.
        let ctx = Context::synthetic(Some("game.exe".into()), None, None, None, None);
        let shell = ScriptRef {
            id: "echo neuron".into(),
            kind: ScriptKind::Shell,
        };
        assert!(run_script_ctx(&shell, &ctx).contains("[disarmed]"));
        let file = ScriptRef {
            id: "C:/does/not/exist.ps1".into(),
            kind: ScriptKind::File,
        };
        assert!(run_script_ctx(&file, &ctx).contains("[disarmed]"));
        let python = ScriptRef {
            id: "any_macro".into(),
            kind: ScriptKind::Python,
        };
        assert!(run_script_ctx(&python, &ctx).contains("[disarmed]"));
    }
}
