//! State-change CONFIRMATIONS — the structural heart of the notification engine.
//!
//! A confirmation is the byproduct of a change that ALREADY committed: a verified device write, a
//! profile that applied, a layer that engaged. It is never an *announcement*. There is no public
//! "post this text" entry point — a [`Confirmation`] is minted only by the typed constructors in
//! this module ([`dpi`], [`profile`], [`layer`], …), and those are called only from the post-commit
//! sites (`intent.rs`, the layer edges, the macro fire path). That is the whole anti-Synapse
//! guarantee: you cannot fire a notice without a real change having landed first.
//!
//! Emission is decoupled from any consumer. Sites call a constructor, which calls [`emit`]; whoever
//! cares — the app's notification engine, or a plain log sink during bring-up — registers a channel
//! with [`set_sink`] and drains it on its own thread (so the engine owns its own lifecycle clock,
//! never the hot path). With no sink registered (CLI, tests) emission is a silent no-op: the core
//! never depends on a consumer existing.

use std::sync::mpsc::Sender;
use std::sync::{Mutex, OnceLock};

/// What changed. Drives the per-event config gate AND the visual/sonic leitmotif — one identity
/// shared across the light (hue) and the sound (tonal centre).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Dpi,
    Scroll,
    Polling,
    Brightness,
    Profile,
    Layer,
    Macro,
}

impl Kind {
    /// Stable slug — the config key and the leitmotif identity seed.
    pub fn slug(self) -> &'static str {
        match self {
            Kind::Dpi => "dpi",
            Kind::Scroll => "scroll",
            Kind::Polling => "polling",
            Kind::Brightness => "brightness",
            Kind::Profile => "profile",
            Kind::Layer => "layer",
            Kind::Macro => "macro",
        }
    }
}

/// How the value reads: a number on a known track (earns a bar) or a bare state (earns a label).
#[derive(Clone, Debug, PartialEq)]
pub enum Shape {
    /// A value on a known track — DPI, polling, brightness, sensitivity. `min`/`max` let the
    /// surface draw a fill; `unit` labels it ("DPI", "Hz", "%").
    Ranged {
        value: f64,
        min: f64,
        max: f64,
        unit: &'static str,
    },
    /// A bare state — a profile name, a layer name, a macro's description.
    Discrete { label: String },
}

/// One confirmed change, ready for the engine to gate, coalesce, and render.
#[derive(Clone, Debug)]
pub struct Confirmation {
    pub kind: Kind,
    pub shape: Shape,
    /// The noun that changed, for the card title ("DPI", "Profile", "Layer on").
    pub title: String,
    /// Coalesce identity: a fresh confirmation with the same `ident` as a live card updates it in
    /// place (cycling DPI is one card that moves, never a stack of five).
    pub ident: String,
    /// The prior value when known, for an old→new read.
    pub prev: Option<String>,
}

static SINK: OnceLock<Mutex<Option<Sender<Confirmation>>>> = OnceLock::new();

fn cell() -> &'static Mutex<Option<Sender<Confirmation>>> {
    SINK.get_or_init(|| Mutex::new(None))
}

/// Register (or, with `None`, detach) the consumer of confirmations. The app's notification engine
/// passes the producing end of its channel here; a bring-up log sink does the same. Replaces any
/// prior sink.
pub fn set_sink(tx: Option<Sender<Confirmation>>) {
    if let Ok(mut g) = cell().lock() {
        *g = tx;
    }
}

/// Deliver a confirmation to the registered sink, if any. The `Sender` is cloned out from under the
/// lock before sending, so no consumer code ever runs while the lock is held. No-op when no sink is
/// registered. This is the single emission path; everything above it is a typed constructor.
pub fn emit(c: Confirmation) {
    let tx = cell().lock().ok().and_then(|g| g.clone());
    if let Some(tx) = tx {
        let _ = tx.send(c);
    }
}

// ── typed constructors: the ONLY way a confirmation comes into being ─────────────────────────────

/// DPI committed to `value` (was `prev`), on the 100..30000 track.
pub fn dpi(value: u32, prev: Option<u32>) {
    emit(Confirmation {
        kind: Kind::Dpi,
        shape: Shape::Ranged {
            value: value as f64,
            min: 100.0,
            max: 30_000.0,
            unit: "DPI",
        },
        title: "DPI".into(),
        ident: "dpi".into(),
        prev: prev.map(|p| p.to_string()),
    });
}

/// Polling rate committed to `hz` (was `prev`), on the 125..8000 track.
pub fn polling(hz: u32, prev: Option<u32>) {
    emit(Confirmation {
        kind: Kind::Polling,
        shape: Shape::Ranged {
            value: hz as f64,
            min: 125.0,
            max: 8_000.0,
            unit: "Hz",
        },
        title: "Polling".into(),
        ident: "polling".into(),
        prev: prev.map(|p| p.to_string()),
    });
}

/// Brightness committed to `pct` (was `prev`), on the 0..100 track.
pub fn brightness(pct: u32, prev: Option<u32>) {
    emit(Confirmation {
        kind: Kind::Brightness,
        shape: Shape::Ranged {
            value: pct as f64,
            min: 0.0,
            max: 100.0,
            unit: "%",
        },
        title: "Brightness".into(),
        ident: "brightness".into(),
        prev: prev.map(|p| p.to_string()),
    });
}

/// A sensitivity / scroll stage committed to `value` on a 0..`max` track.
pub fn scroll(value: u32, max: u32, prev: Option<u32>) {
    emit(Confirmation {
        kind: Kind::Scroll,
        shape: Shape::Ranged {
            value: value as f64,
            min: 0.0,
            max: max.max(1) as f64,
            unit: "",
        },
        title: "Sensitivity".into(),
        ident: "scroll".into(),
        prev: prev.map(|p| p.to_string()),
    });
}

/// The active profile is now `name` (was `prev`).
pub fn profile(name: &str, prev: Option<&str>) {
    emit(Confirmation {
        kind: Kind::Profile,
        shape: Shape::Discrete {
            label: name.to_string(),
        },
        title: "Profile".into(),
        ident: "profile".into(),
        prev: prev.map(|s| s.to_string()),
    });
}

/// A HyperShift layer engaged (`engaged = true`) or released. Distinct layers get distinct idents
/// so engaging one doesn't collapse another's card.
pub fn layer(name: &str, engaged: bool) {
    emit(Confirmation {
        kind: Kind::Layer,
        shape: Shape::Discrete {
            label: name.to_string(),
        },
        title: if engaged { "Layer on" } else { "Layer off" }.into(),
        ident: format!("layer:{name}"),
        prev: None,
    });
}

/// A macro fired. `desc` is its result line; opt-in per binding, so this is only ever called when a
/// binding asked to be confirmed.
pub fn macro_fired(desc: &str) {
    emit(Confirmation {
        kind: Kind::Macro,
        shape: Shape::Discrete {
            label: desc.to_string(),
        },
        title: "Macro".into(),
        ident: format!("macro:{desc}"),
        prev: None,
    });
}
