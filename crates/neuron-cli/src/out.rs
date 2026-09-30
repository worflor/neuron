// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

//! Output shared by every verb: `--json` switches a verb from human text to one JSON document on
//! stdout. Reads emit their data; writes emit the state they produced (read back from disk), so a
//! script never has to guess whether an edit landed. Errors go to stderr with a nonzero exit.

use anyhow::Result;
use serde::Serialize;
use std::sync::atomic::{AtomicBool, Ordering};

static JSON: AtomicBool = AtomicBool::new(false);
static VERBOSE: AtomicBool = AtomicBool::new(false);

pub fn set_verbose(on: bool) {
    VERBOSE.store(on, Ordering::Relaxed);
}

/// Engine chatter (host start-up lines and the like) is shown only with `--verbose` or `NEURON_DEBUG`.
#[must_use]
pub fn verbose() -> bool {
    VERBOSE.load(Ordering::Relaxed) || std::env::var_os("NEURON_DEBUG").is_some_and(|v| !v.is_empty())
}

pub fn set_json(on: bool) {
    JSON.store(on, Ordering::Relaxed);
}

#[must_use]
pub fn json() -> bool {
    JSON.load(Ordering::Relaxed)
}

/// Progress text for a human: printed normally, silent under `--json`, where the verb's final
/// document carries the result instead.
#[macro_export]
macro_rules! say {
    ($($t:tt)*) => {
        if !$crate::out::json() {
            println!($($t)*);
        }
    };
}

/// Print one JSON document, pretty.
pub fn print_json(v: &serde_json::Value) {
    match serde_json::to_string_pretty(v) {
        Ok(s) => println!("{s}"),
        Err(e) => eprintln!("error: could not render JSON: {e}"),
    }
}

/// Emit `value` as JSON under `--json`, else run `human`.
pub fn emit(value: &impl Serialize, human: impl FnOnce()) -> Result<()> {
    if json() {
        print_json(&serde_json::to_value(value)?);
    } else {
        human();
    }
    Ok(())
}

/// Emit a write's result: under `--json` the document, otherwise one status line.
pub fn done(value: serde_json::Value, line: impl AsRef<str>) {
    if json() {
        print_json(&value);
    } else {
        println!("{}", line.as_ref());
    }
}

/// Print advisory text on stderr (never mixed into JSON on stdout).
pub fn note(line: impl AsRef<str>) {
    eprintln!("{}", line.as_ref());
}

/// The wire error shape: `{"error": "..."}` on stderr under `--json`.
pub fn error_line(e: &anyhow::Error) -> String {
    if json() {
        serde_json::json!({ "error": format!("{e:#}") }).to_string()
    } else {
        format!("error: {e:#}")
    }
}
