// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

//! `neuron macro`: the Python macros under `macros/scripts/`. List, read, create or replace from a
//! file, stdin or text, delete, syntax-check, run once (safe by default), and set the options a
//! macro declares. A macro is bound to a trigger with `neuron bind add --action macro:NAME`.

use crate::{live, out};
use anyhow::{bail, Context, Result};
use clap::Subcommand;
use neuron::macros::{self, macro_host, macro_host::macro_host as host};
use serde_json::{json, Value};
use std::fmt::Write as _;

#[derive(Subcommand)]
pub enum MacroCmd {
    /// List the macros with their authority mode
    List,
    /// Print a macro's source
    Show { name: String },
    /// Create or replace a macro from a file, stdin (`-`) or `--source`; it is checked, then
    /// registered warm so a syntax error surfaces now
    Add {
        /// macro name: ASCII letters, digits, `_` and `-`
        name: String,
        /// a .py file, or `-` for stdin
        file: Option<String>,
        /// the source as an argument
        #[arg(long, conflicts_with = "file")]
        source: Option<String>,
        /// stamp the authority directive: `bound` (default, brokered helpers) or `raw` (full Python)
        #[arg(long)]
        mode: Option<String>,
        /// write the file without running the Python check (no runtime needed)
        #[arg(long)]
        no_check: bool,
    },
    /// Delete a macro (and report any bind that still names it)
    Rm { name: String },
    /// Switch a stored macro between `bound` and `raw` authority
    Mode { name: String, mode: String },
    /// Syntax-check a macro without executing it: a stored one, a file, or stdin (`-`)
    Check {
        /// a .py file, or `-` for stdin
        file: Option<String>,
        /// a stored macro instead
        #[arg(long, conflicts_with = "file")]
        name: Option<String>,
    },
    /// Run a macro ONCE against the live context and print the result. Input helpers are traced,
    /// not fired, unless `--arm`.
    Run {
        /// a stored macro
        name: Option<String>,
        /// or run a .py file directly (not stored)
        #[arg(long)]
        file: Option<String>,
        /// arm real input synthesis for this run
        #[arg(long)]
        arm: bool,
    },
    /// A macro's declared options and chosen values; `--set '{"key": value}'` chooses
    Options {
        name: String,
        #[arg(long)]
        set: Option<String>,
    },
    /// Print the `neuron` host-module reference a macro can call
    Prelude,
}

fn mode_of(src: &str) -> &'static str {
    match macros::mode_from_source(src) {
        Ok(macros::MacroMode::Raw) => "raw",
        Ok(macros::MacroMode::Bound) => "bound",
        Err(_) => "invalid",
    }
}

fn parse_mode(s: &str) -> Result<macros::MacroMode> {
    match s.to_ascii_lowercase().as_str() {
        "raw" => Ok(macros::MacroMode::Raw),
        "bound" => Ok(macros::MacroMode::Bound),
        other => bail!("mode must be 'bound' or 'raw', not '{other}'"),
    }
}

fn read_source(file: Option<&str>, inline: Option<&str>) -> Result<String> {
    use std::io::Read as _;
    match (file, inline) {
        (Some("-"), _) => {
            let mut s = String::new();
            std::io::stdin().read_to_string(&mut s)?;
            Ok(s)
        }
        (Some(f), _) => std::fs::read_to_string(f).with_context(|| format!("reading macro source {f}")),
        (None, Some(s)) => Ok(s.to_string()),
        (None, None) => bail!("give the source: a file path, `-` for stdin, or --source TEXT"),
    }
}

/// The names of every bind (gui + every profile) that runs macro `name`.
fn referenced_by(name: &str) -> Vec<String> {
    use neuron::action::{Action, ScriptKind};
    use neuron::authoring::RuleStore;
    let names_it = |a: &Action| matches!(a, Action::Script { script } if script.kind == ScriptKind::Python && script.id == name);
    let mut out = Vec::new();
    let mut stores = vec![RuleStore::Gui];
    stores.extend(neuron::profile::list().into_iter().map(RuleStore::Profile));
    for s in stores {
        for (i, r) in s.load().unwrap_or_default().iter().enumerate() {
            if names_it(&r.action) {
                out.push(format!("{}[{i}]", s.label()));
            }
        }
    }
    let cast = neuron::cast::CastConfig::load();
    for (i, a) in cast.radial.iter().enumerate() {
        if names_it(a) {
            out.push(format!("cast.radial[{i}]"));
        }
    }
    for (g, a) in &cast.gestures {
        if names_it(a) {
            out.push(format!("cast.gestures.{g}"));
        }
    }
    out
}

pub fn run(cmd: MacroCmd) -> Result<()> {
    match cmd {
        MacroCmd::Prelude => {
            print!("{}", macros::pyruntime::host_module_source());
            Ok(())
        }
        MacroCmd::List => {
            let dir = macro_host::macros_dir();
            let rows: Vec<Value> = macro_host::list_macros()
                .into_iter()
                .map(|n| {
                    let src = macro_host::load_macro(&n).unwrap_or_default();
                    json!({ "name": n, "mode": mode_of(&src), "bytes": src.len(), "path": dir.join(format!("{n}.py")).display().to_string() })
                })
                .collect();
            let runtime = host().available();
            out::emit(&json!({ "dir": dir.display().to_string(), "runtime_available": runtime, "macros": rows }), || {
                println!("python macros (in {}):", dir.display());
                if rows.is_empty() {
                    println!("  (none yet: `neuron macro add NAME file.py`)");
                }
                for r in &rows {
                    println!("  {:<24} {}", r["name"].as_str().unwrap_or(""), r["mode"].as_str().unwrap_or(""));
                }
                if !runtime {
                    println!("note: no python runtime resolved: macros are stored but cannot run here.");
                }
            })
        }
        MacroCmd::Show { name } => {
            let Some(src) = macro_host::load_macro(&name) else { bail!("no macro '{name}' (neuron macro list)") };
            out::emit(&json!({ "name": name, "mode": mode_of(&src), "source": src }), || print!("{src}"))
        }
        MacroCmd::Add { name, file, source, mode, no_check } => {
            macro_host::validate_macro_id(&name).map_err(anyhow::Error::msg)?;
            let mut src = read_source(file.as_deref(), source.as_deref())?;
            if let Some(m) = mode {
                src = macros::set_source_mode(&src, parse_mode(&m)?);
            }
            macros::mode_from_source(&src).map_err(anyhow::Error::msg)?;
            let existed = macro_host::load_macro(&name).is_some();
            if no_check {
                macro_host::write_macro_file(&name, &src).map_err(anyhow::Error::msg)?;
            } else {
                host().register(&name, &src).map_err(|e| anyhow::anyhow!("register '{name}': {e}"))?;
            }
            live::finish(
                json!({ "name": name, "mode": mode_of(&src), "bytes": src.len(), "replaced": existed, "checked": !no_check }),
                format!("{} macro '{name}' ({}{})", if existed { "replaced" } else { "added" }, mode_of(&src), if no_check { ", unchecked" } else { ", checked + warm" }),
            );
            Ok(())
        }
        MacroCmd::Rm { name } => {
            if macro_host::load_macro(&name).is_none() {
                bail!("no macro '{name}' (neuron macro list)");
            }
            let refs = referenced_by(&name);
            macro_host::delete_macro(&name)?;
            if !refs.is_empty() {
                out::note(format!("warning: still named by {}; those binds will report the macro missing", refs.join(", ")));
            }
            live::finish(json!({ "removed": name, "still_referenced_by": refs }), format!("deleted macro '{name}'"));
            Ok(())
        }
        MacroCmd::Mode { name, mode } => {
            let Some(src) = macro_host::load_macro(&name) else { bail!("no macro '{name}'") };
            let next = macros::set_source_mode(&src, parse_mode(&mode)?);
            if next == src {
                return out::emit(&json!({ "name": name, "mode": mode_of(&src), "changed": false }), || println!("'{name}' is already {}", mode_of(&src)));
            }
            host().register(&name, &next).map_err(|e| anyhow::anyhow!("register '{name}': {e}"))?;
            live::finish(json!({ "name": name, "mode": mode_of(&next), "changed": true }), format!("'{name}' is now {}", mode_of(&next)));
            Ok(())
        }
        MacroCmd::Check { file, name } => {
            let src = match (&file, &name) {
                (None, Some(n)) => macro_host::load_macro(n).with_context(|| format!("no macro '{n}'"))?,
                _ => read_source(file.as_deref(), None)?,
            };
            match host().check(&src) {
                Ok(defs) => {
                    let has_entry = defs.iter().any(|d| d == "macro" || d == "main");
                    out::emit(&json!({ "ok": true, "defs": defs, "has_entry_point": has_entry, "mode": mode_of(&src) }), || {
                        println!("ok, defines: {}", if defs.is_empty() { "(none)".into() } else { defs.join(", ") });
                        if !has_entry {
                            println!("warning: no `def macro(ctx):` (or `def main(ctx):`) entry point");
                        }
                    })
                }
                Err(e) => bail!("check: {e}"),
            }
        }
        MacroCmd::Options { name, set } => {
            if macro_host::load_macro(&name).is_none() {
                bail!("no macro '{name}'");
            }
            if let Some(s) = set {
                let v: Value = serde_json::from_str(&s).context("--set takes a JSON object, e.g. '{\"volume\": 3}'")?;
                if !v.is_object() {
                    bail!("--set takes a JSON object");
                }
                host().set_option_values(&name, &v).map_err(anyhow::Error::msg)?;
            }
            let manifest = host().options_manifest(&name);
            let values = host().option_values(&name);
            out::emit(&json!({ "name": name, "manifest": manifest, "values": values }), || {
                println!("options for '{name}': {values}");
                if manifest.is_none() {
                    println!("(declared options are read when the macro is registered warm; `neuron macro add` registers it)");
                }
            })
        }
        MacroCmd::Run { name, file, arm } => run_once(name, file, arm),
    }
}

/// Run a macro once. Beacons (`neuron.ask`) become y/n prompts on the terminal.
fn run_once(name: Option<String>, file: Option<String>, arm: bool) -> Result<()> {
    neuron::action::arm_input(arm);
    host().set_armed(arm);
    let (id, src) = match (name, file) {
        (_, Some(f)) => {
            let src = std::fs::read_to_string(&f).with_context(|| format!("reading {f}"))?;
            let id = std::path::Path::new(&f).file_stem().and_then(|s| s.to_str()).unwrap_or("adhoc").to_string();
            (id, Some(src))
        }
        (Some(n), None) => {
            let known = macro_host::list_macros();
            if !known.contains(&n) {
                match neuron::authoring::nearest(&n, known.iter().map(String::as_str)) {
                    Some(s) => bail!("no macro named '{n}'. Did you mean {s}?"),
                    None => bail!("no macro named '{n}'. Run `neuron macro list` to see them."),
                }
            }
            (n, None)
        }
        (None, None) => bail!("pass a macro name or --file"),
    };
    let beacons = host().beacon_events();
    std::thread::spawn(move || {
        use neuron::macros::BeaconEvent;
        while let Ok(ev) = beacons.recv() {
            match ev {
                BeaconEvent::Ask { pid, macro_id, text, .. } => {
                    eprintln!("[beacon] {macro_id} asks: {text}  [y/n, enter = dismiss]");
                    let mut line = String::new();
                    let ans = match std::io::stdin().read_line(&mut line) {
                        Ok(_) => match line.trim_matches(|c: char| c.is_whitespace() || c == '\u{feff}').to_ascii_lowercase().as_str() {
                            "y" | "yes" => Some(0_usize),
                            "n" | "no" => Some(1_usize),
                            _ => None,
                        },
                        Err(_) => None,
                    };
                    host().answer(pid, ans);
                }
                BeaconEvent::Notify { macro_id, text } => eprintln!("[{macro_id}] {text}"),
                BeaconEvent::Retire { .. } | BeaconEvent::RetireDomain { .. } => {}
            }
        }
    });
    let ctx = macros::Context::capture();
    let budget = std::time::Duration::from_mins(10);
    let result = match src.as_deref() {
        Some(source) => host().invoke_source_with_budget(&id, source, &ctx, budget),
        None => host().invoke_with_budget(&id, &ctx, budget),
    };
    let log: Vec<String> = host().drain_log().into_iter().filter(|l| out::verbose() || !l.starts_with("[macro host]")).collect();
    if let Some(why) = run_failure(&result) {
        let short = if why.contains("Traceback") && !out::verbose() { why.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or(&why).trim().to_string() } else { why };
        let mut msg = format!("macro '{id}' failed: {short}");
        if !out::json() {
            for line in &log {
                let _ = write!(msg, "\n  | {line}");
            }
        }
        bail!(msg);
    }
    out::emit(&json!({ "macro": id, "armed": arm, "result": result, "log": log }), || {
        println!("{result}");
        for line in &log {
            println!("  | {line}");
        }
    })
}

/// Why a run failed, read from the host's one-line result, or `None` when it succeeded. The
/// host reports failures as text: `[reason]`, `macro 'x' error: reason`, or a wait that expired.
fn run_failure(result: &str) -> Option<String> {
    let r = result.trim();
    if let Some(inner) = r.strip_prefix('[').and_then(|x| x.strip_suffix(']')) {
        return Some(inner.to_string());
    }
    if let Some(rest) = r.strip_prefix("macro '") {
        if let Some((_, why)) = rest.split_once("' error: ") {
            return Some(why.to_string());
        }
        if rest.contains("' still running") {
            return Some("it was still running when the wait ended; see the macro log".into());
        }
        if rest.ends_with("' is not registered") || rest.ends_with("' queue full") {
            return Some(rest.split_once("' ").map_or(rest, |(_, w)| w).to_string());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_host_result_line_is_a_failure_only_when_it_says_so() {
        assert_eq!(run_failure("macro 'nope' error: macro 'nope' not registered").as_deref(), Some("macro 'nope' not registered"));
        assert_eq!(run_failure("[sidecar pipe broken]").as_deref(), Some("sidecar pipe broken"));
        assert!(run_failure("macro 'a' ran").is_none());
        assert!(run_failure("macro 'a': done").is_none());
    }

    #[test]
    fn modes_read_from_the_leading_directive() {
        assert_eq!(mode_of("def macro(ctx):\n    pass\n"), "bound");
        assert_eq!(mode_of("# neuron: raw\ndef macro(ctx):\n    pass\n"), "raw");
        assert!(parse_mode("raw").is_ok() && parse_mode("BOUND").is_ok());
        assert!(parse_mode("root").is_err());
    }

    #[test]
    fn sources_come_from_text_or_a_file_and_never_from_nothing() {
        assert_eq!(read_source(None, Some("x = 1")).unwrap(), "x = 1");
        assert!(read_source(None, None).is_err());
        assert!(read_source(Some("/no/such/file.py"), None).is_err());
    }
}
