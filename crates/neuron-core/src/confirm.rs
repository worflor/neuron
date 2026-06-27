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

use std::sync::atomic::{AtomicU32, Ordering};
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
    Battery,
    /// A swappable SIDE PLATE was attached / detached (the device pushes its strap-code; there is no
    /// getter). A discrete hardware-piece confirmation, distinct from the power/battery family.
    SidePlate,
}

impl Kind {
    /// Every kind, in declaration order — the canonical list to iterate (config gates, the test
    /// probe, …). Call sites derive from this instead of hardcoding a subset, so a new variant is
    /// picked up everywhere by adding one arm here (and to [`Kind::slug`]).
    pub const ALL: [Kind; 9] = [
        Kind::Dpi,
        Kind::Scroll,
        Kind::Polling,
        Kind::Brightness,
        Kind::Profile,
        Kind::Layer,
        Kind::Macro,
        Kind::Battery,
        Kind::SidePlate,
    ];
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
            Kind::Battery => "battery",
            Kind::SidePlate => "side_plate",
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

// De-dup baseline for OBSERVED changes (the events a device PUSHES — e.g. an onboard DPI button).
// A device double-sends each event AND may echo a host-initiated write, so naive emission would
// double-card. Every confirmation records the value it carried here; an OBSERVED event only fires if
// it differs. This unifies the two sources: a host write (`dpi`/`scroll`) updates the baseline too, so
// the device's echo of it is absorbed, while a genuine onboard change still differs and cards. `0` =
// "unknown yet" (no confirmation seen since launch).
static LAST_DPI: AtomicU32 = AtomicU32::new(0);
static LAST_SCROLL: AtomicU32 = AtomicU32::new(0);

// Side-plate de-dup baseline + last-known label. The plate is detected ONLY by the report the device
// PUSHES on a swap (no getter). The seating BOUNCE (the strap contact flickering mid-seat) is absorbed
// UPSTREAM by a trailing time-debounce in `hidwatch` — this module stays PURE (value-dedup only, no
// timing/threads), so it only ever sees the SETTLED strap-code. We then card only when that code
// differs from the last known (a double-send is still swallowed). A DETACH (id `0`) is a normal beat
// of every swap — the plate physically leaves before the new one seats — so it updates the readout
// SILENTLY and fires NO card (carding "disconnected" each swap is noise); a real plate (id != 0) cards.
// `PLATE_UNKNOWN` (an impossible strap-code) is the "nothing observed yet" sentinel.
// `LAST_PLATE_LABEL` carries the resolved label for the GUI readout (the plate has no getter to poll).
const PLATE_UNKNOWN: u32 = u32::MAX;
static LAST_PLATE: AtomicU32 = AtomicU32::new(PLATE_UNKNOWN);
static LAST_PLATE_LABEL: Mutex<Option<String>> = Mutex::new(None);

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
    LAST_DPI.store(value, Ordering::Relaxed); // baseline for device-event de-dup
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
    LAST_SCROLL.store(value, Ordering::Relaxed); // baseline for device-event de-dup
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

/// A device-power confirmation — battery `pct` with an event-specific `title` ("Charging", "On
/// battery", "Battery low", "Battery critical", "Fully charged"). All share the `battery` ident so
/// they coalesce into one card that updates rather than stacking, and `pct` drives the fill bar. The
/// edge policy (which title, when) lives in [`crate::vitals`].
pub fn battery(pct: u32, title: &str, prev: Option<u32>) {
    emit(Confirmation {
        kind: Kind::Battery,
        shape: Shape::Ranged {
            value: pct as f64,
            min: 0.0,
            max: 100.0,
            unit: "%",
        },
        title: title.into(),
        ident: "battery".into(),
        prev: prev.map(|p| p.to_string()),
    });
}

/// A swappable SIDE PLATE is now `label` ("12-button", "6-button", "detached", …). A discrete
/// hardware-piece confirmation; all plate changes share the `side_plate` ident so a swap coalesces
/// into one card that updates in place (a settle-transient is overwritten by the settled value rather
/// than stacking). Minted only via [`observe_side_plate`] off the device's pushed report.
pub fn side_plate(label: &str) {
    emit(Confirmation {
        kind: Kind::SidePlate,
        shape: Shape::Discrete {
            label: label.to_string(),
        },
        title: "Side plate".into(),
        ident: "side_plate".into(),
        prev: None,
    });
}

// ── OBSERVED changes: the device pushed a state it changed ON ITS OWN (an onboard button) ─────────
// These are the entry points the HID event listener calls. They card ONLY on a real change vs the
// last value we know about — so the device's double-send, and any echo of a host write, are silent.

/// The device reported its DPI is now `value` (e.g. you pressed its onboard DPI button). Cards it
/// only if it differs from the last DPI we confirmed (host writes share that baseline). The `prev` of
/// a first-ever observation is unknown, so it reads as a bare new value rather than a false old→new.
pub fn observe_dpi(value: u32) {
    let prev = LAST_DPI.load(Ordering::Relaxed);
    if value == prev {
        return;
    }
    dpi(value, if prev == 0 { None } else { Some(prev) });
}

/// The device reported its scroll/sensitivity stage is now `stage` (its onboard stage button), on a
/// 1..`max` track. Same de-dup contract as [`observe_dpi`].
pub fn observe_scroll(stage: u32, max: u32) {
    let prev = LAST_SCROLL.load(Ordering::Relaxed);
    if stage == prev {
        return;
    }
    scroll(stage, max, if prev == 0 { None } else { Some(prev) });
}

/// The device reported a swappable SIDE PLATE change: `id` is the raw hardware strap-code, `label`
/// its already-resolved human form (resolved DATA-side via the registry's `[side_plates]` map at the
/// decode site; `0`/detached resolves to "detached"). The mid-seat seating BOUNCE is debounced upstream
/// in `hidwatch`, so this only ever sees the settled code. It ALWAYS updates the readout (the
/// `LAST_PLATE_LABEL` the GUI polls, since the plate has no getter), but emits a confirmation CARD only
/// when the code BOTH differs from the last one we saw (a double-send is swallowed) AND is a real plate
/// (`id != 0`): a detach is the silent "no plate" beat of a swap, not its own notice.
pub fn observe_side_plate(id: u32, label: &str) {
    let prev = LAST_PLATE.swap(id, Ordering::Relaxed);
    // Always keep the readout honest — even a same-code re-report or a detach refreshes the label.
    if let Ok(mut g) = LAST_PLATE_LABEL.lock() {
        *g = Some(label.to_string());
    }
    if id == prev {
        return; // same plate as last known (a double-send) — readout current, no card
    }
    // A detach (id 0) updates the readout above but fires no card; a real plate cards once.
    if id != 0 {
        side_plate(label);
    }
}

/// The last side-plate label the device pushed, if any has been observed since launch. Surfaced on
/// the DEVICE page (the plate is push-only, so this last-known value is the honest readout). `None`
/// until the first observation.
pub fn last_plate() -> Option<String> {
    LAST_PLATE_LABEL.lock().ok().and_then(|g| g.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    // The emission sink + the plate baseline are process-global, so any test that drives an `observe`
    // and reads the sink must run alone. This lock serializes them within the lib-test binary.
    // Acquired poison-tolerantly (below): if a sibling side-plate test transiently panics under heavy
    // parallel load it must not poison the lock and cascade-fail the others — the guarded state is reset
    // at the top of every `capture_side_plates`, so inheriting it after a poison is harmless.
    static TEST_LOCK: Mutex<()> = Mutex::new(());

    /// Drive `f` with a fresh plate baseline + a private sink, and return the SIDE-PLATE confirmations
    /// it produced PLUS a snapshot of `last_plate()` read UNDER the lock (other kinds a concurrent test
    /// might emit are filtered out). The plate snapshot is taken HERE, inside the critical section — not
    /// by the caller after the guard drops — so a sibling side-plate test can't race the global
    /// `LAST_PLATE_LABEL` between this returning and the assertion. That post-lock read was the real
    /// cause of the rare parallel-run flake; asserting on this returned snapshot removes it.
    fn capture_side_plates(f: impl FnOnce()) -> (Vec<Confirmation>, Option<String>) {
        let _g = TEST_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        LAST_PLATE.store(PLATE_UNKNOWN, Ordering::Relaxed);
        *LAST_PLATE_LABEL.lock().unwrap() = None;
        let (tx, rx) = mpsc::channel();
        set_sink(Some(tx));
        f();
        set_sink(None);
        let fired: Vec<Confirmation> = rx.try_iter().filter(|c| c.kind == Kind::SidePlate).collect();
        (fired, last_plate())
    }

    #[test]
    fn side_plate_dedups_repeats() {
        // same plate reported twice (the device double-sends) → exactly ONE card.
        let (fired, plate) = capture_side_plates(|| {
            observe_side_plate(3, "12-button");
            observe_side_plate(3, "12-button");
        });
        assert_eq!(fired.len(), 1, "a repeated strap-code must card only once");
        match &fired[0].shape {
            Shape::Discrete { label } => assert_eq!(label, "12-button"),
            other => panic!("side plate is a discrete confirmation, got {other:?}"),
        }
        assert_eq!(plate.as_deref(), Some("12-button"));
    }

    #[test]
    fn side_plate_transient_is_absorbed_settled_fires() {
        // 6-button seated; a swap to 12-button momentarily re-reads the OLD code mid-seat (transient,
        // == last known → absorbed) before it settles on the new code (fires once). The card reflects
        // the SETTLED plate, never the transient.
        let (fired, plate) = capture_side_plates(|| {
            observe_side_plate(4, "6-button"); // currently seated (the "before")
            observe_side_plate(4, "6-button"); // mid-seat transient — same as last → no card
            observe_side_plate(3, "12-button"); // settled — differs → one card
        });
        // two distinct strap-codes were seen (6-button, then 12-button); the transient added none.
        let labels: Vec<&str> = fired
            .iter()
            .filter_map(|c| match &c.shape {
                Shape::Discrete { label } => Some(label.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(labels, vec!["6-button", "12-button"]);
        assert_eq!(plate.as_deref(), Some("12-button"), "settled plate wins");
    }

    #[test]
    fn side_plate_detached_updates_readout_without_carding() {
        // a detach (id 0) is the normal "no plate" beat of every swap — it must UPDATE the readout but
        // fire NO card, so a swap doesn't spam a "plate disconnected" notice the user dislikes.
        let (fired, plate) = capture_side_plates(|| observe_side_plate(0, "detached"));
        assert!(fired.is_empty(), "a detach must not card");
        assert_eq!(plate.as_deref(), Some("detached"), "but the readout still updates");
    }

    #[test]
    fn side_plate_swap_cards_only_the_real_plate_not_the_detach() {
        // a full swap: the old plate is seated, it detaches mid-swap (silent), then the new plate
        // seats (cards once). The detach in the middle updates the readout but adds no card.
        let (fired, plate) = capture_side_plates(|| {
            observe_side_plate(4, "6-button"); // seated before — cards
            observe_side_plate(0, "detached"); // mid-swap detach — silent, readout only
            observe_side_plate(3, "12-button"); // new plate seats — cards
        });
        let labels: Vec<&str> = fired
            .iter()
            .filter_map(|c| match &c.shape {
                Shape::Discrete { label } => Some(label.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(labels, vec!["6-button", "12-button"], "the detach fires no card");
        assert_eq!(plate.as_deref(), Some("12-button"), "settled plate wins the readout");
    }
}
