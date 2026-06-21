//! The macro engine — the power tier of the spine.
//!
//! A Neuron macro is **Python**, run by the [`MacroHost`] — a bundled private CPython kept warm as a
//! sidecar process. Macros do LITERALLY ANYTHING a program can (real CPython: `ctypes` into raw
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

pub use context::Context;
pub use macro_host::{macro_host, BeaconEvent, MacroHost};

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
pub fn spawn_shell_command(cmdline: &str) -> std::io::Result<Child> {
    let cmd = if cfg!(windows) {
        let mut c = Command::new("cmd");
        c.args(["/C", cmdline]);
        c
    } else {
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
pub fn run_script(script: &ScriptRef) -> String {
    run_script_ctx(script, &Context::capture())
}

/// Context-aware dispatch — the spine's path. A [`ScriptKind::Python`] macro is FIRED ASYNC against
/// the trigger-time `ctx` (never blocking the dispatch thread — the result lands in the macro log);
/// Shell/File shell out and ignore `ctx`.
pub fn run_script_ctx(script: &ScriptRef, ctx: &Context) -> String {
    match script.kind {
        ScriptKind::Python => macro_host().fire_async(&script.id, ctx),
        ScriptKind::Shell => run_shell(&script.id),
        ScriptKind::File => run_external(&script.id),
    }
}

/// Run a python macro by id and WAIT (bounded) for its result — the GUI "test run" + CLI path.
/// NEVER call from the input/UI thread.
pub fn test_python_macro(id: &str, ctx: &Context) -> String {
    macro_host().invoke(id, ctx)
}

// ── shell-out tiers (kept from the old engine; clean and dependency-free) ───────────────────────

/// Run an inline command line through the OS interpreter (cmd on Windows, sh elsewhere) —
/// [`ScriptKind::Shell`]. Fire-and-forget; returns a one-line result. Honors the process-spawn arm
/// gate (disarmed/test/verify => report, spawn nothing) and strips armed authority from the child.
pub fn run_shell(cmd: &str) -> String {
    if !crate::action::process_spawn_armed() {
        return format!("shell `{cmd}` [disarmed]");
    }
    let res = spawn_shell_command(cmd);
    match res {
        Ok(_) => format!("ran `{cmd}`"),
        Err(e) => format!("shell failed: {e}"),
    }
}

/// Run an external script/executable by path (.ps1/.py/.exe) — [`ScriptKind::File`]. Returns a
/// one-line result. Arm-gated like the inline shell-out.
pub fn run_external(path: &str) -> String {
    if !crate::action::process_spawn_armed() {
        return format!("launch `{path}` [disarmed]");
    }
    match launch_by_extension(std::path::Path::new(path)) {
        Ok(_) => format!("launched `{path}`"),
        Err(e) => format!("launch failed: {e}"),
    }
}

/// Dispatch an external file to the right runner by extension (.ps1 -> powershell, .py -> python,
/// anything else -> execute directly). Best-effort. Strips armed authority from the child env.
fn launch_by_extension(path: &std::path::Path) -> std::io::Result<std::process::Child> {
    spawn_external_path(path)
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

    #[test]
    fn run_script_ctx_routes_shell_and_file_without_a_sidecar() {
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
    }
}
