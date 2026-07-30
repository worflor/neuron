// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

//! The OBS act-verb seam.
//!
//! `run_act` (the macro `act` protocol) lives here in core, but the OBS
//! connection lives up in the app (it owns the protocol host). So core exposes
//! this thin sink — the same inversion as [`crate::curtain::set_painter`] and
//! `confirm::set_sink`: the app installs a forwarder at startup, and an
//! `obs_*` macro verb routes through it. With nothing installed (CONNECTIONS
//! off, or no host build), an OBS verb fails honestly rather than pretending.
//!
//! This is what makes OBS control piggyback on everything already built: any
//! macro — and therefore any bindable key, button, or gesture — can switch a
//! scene or toggle the stream, with no new Action variant and no new UI.

use std::sync::{OnceLock, RwLock};

use serde_json::Value;

/// `(verb, arg) -> (ok, message)`. `verb` is the full act verb (`obs_scene`,
/// `obs_stream`, …); `arg` is the macro's JSON argument (a scene name,
/// "toggle", …) — the same `serde_json::Value` `run_act` already threads.
type ObsSink = Box<dyn Fn(&str, &Value) -> (bool, String) + Send + Sync>;

// RwLock, not OnceLock alone: the app re-installs (or clears) the sink as the
// CONNECTIONS toggle opens and closes the OBS connection at runtime.
static OBS_SINK: OnceLock<RwLock<Option<ObsSink>>> = OnceLock::new();

fn cell() -> &'static RwLock<Option<ObsSink>> {
    OBS_SINK.get_or_init(|| RwLock::new(None))
}

/// Install (or replace) the OBS forwarder. The app calls this when it brings
/// the OBS connection up.
pub fn set_sink(f: ObsSink) {
    *cell().write().unwrap_or_else(|e| e.into_inner()) = Some(f);
}

/// Clear the forwarder — OBS verbs then report "not connected". The app calls
/// this when CONNECTIONS (or just OBS) is switched off.
pub fn clear_sink() {
    *cell().write().unwrap_or_else(|e| e.into_inner()) = None;
}

/// Route an OBS verb through the installed sink. `None` = no sink installed
/// (the caller reports "OBS not connected"); `Some((ok,msg))` = the sink ran.
pub fn dispatch(verb: &str, arg: &Value) -> Option<(bool, String)> {
    let g = cell().read().unwrap_or_else(|e| e.into_inner());
    g.as_ref().map(|f| f(verb, arg))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dispatch_is_none_until_installed_then_routes() {
        clear_sink();
        assert!(dispatch("obs_scene", &Value::from("X")).is_none());

        set_sink(Box::new(|verb, arg| {
            (true, format!("{verb}:{}", arg.as_str().unwrap_or("")))
        }));
        assert_eq!(
            dispatch("obs_scene", &Value::from("Gameplay")),
            Some((true, "obs_scene:Gameplay".to_string()))
        );

        clear_sink();
        assert!(dispatch("obs_scene", &Value::from("X")).is_none());
    }
}
