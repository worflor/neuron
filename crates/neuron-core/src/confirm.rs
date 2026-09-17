// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

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

use std::collections::HashMap;
use std::sync::mpsc::Sender;
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

/// What changed. Drives the per-event config gate AND the visual/sonic leitmotif — one identity
/// shared across the light (hue) and the sound (tonal centre).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Dpi,
    /// The sniper HOLD engaged/released — technically a DPI write, deliberately NOT [`Kind::Dpi`]:
    /// the notification model classifies by INTENT, not by which register moved. Turning a knob is
    /// a settings change you asked to hear about; a held instrument doing its job mid-game is not
    /// (the slowed crosshair already confirms it). Its own kind = its own gate, so "tell me about
    /// DPI changes" and "don't ping me mid-clutch" stop being one switch.
    Sniper,
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
    pub const ALL: [Kind; 10] = [
        Kind::Dpi,
        Kind::Sniper,
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
            Kind::Sniper => "sniper",
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

// De-dup baselines for OBSERVED changes (the events a device PUSHES — e.g. an onboard DPI button),
// kept PER PHYSICAL DEVICE (keyed by USB product id). A device double-sends each event AND may echo a
// host-initiated write, so naive emission would double-card. Every confirmation records the value it
// carried against ITS OWN device's baseline; an OBSERVED event only fires if it differs from that
// device's last value. This unifies the two sources for one device — a host write (`dpi`/`scroll`)
// updates that device's baseline too, so the device's echo of it is absorbed, while a genuine onboard
// change still differs and cards — and KEEPS DEVICES APART: mouse A's burst can never overwrite mouse
// B's baseline, so B's later genuine change is neither suppressed nor misattributed (vitals is already
// per-pid; this brings the settings/confirm path to parity).
//
// The side plate is detected ONLY by the report the device PUSHES on a swap (no getter). The seating
// BOUNCE (the strap contact flickering mid-seat) is absorbed UPSTREAM by a trailing time-debounce in
// `hidwatch` — this module stays PURE (value-dedup only, no timing/threads), so it only ever sees the
// SETTLED strap-code. We card only when that code differs from this device's last known (a double-send
// is swallowed). A DETACH (id `0`) is a normal beat of every swap — the plate physically leaves before
// the new one seats — so it updates the readout SILENTLY and fires NO card; a real plate (id != 0)
// cards. `plate_label` carries the resolved label for the GUI readout (the plate has no getter to poll).

/// An impossible strap-code — the "nothing observed yet" plate sentinel.
const PLATE_UNKNOWN: u32 = u32::MAX;

/// One physical device's de-dup baselines. `dpi`/`scroll` of `0` = "unknown yet" (no confirmation seen
/// for this pid since launch); `plate` of [`PLATE_UNKNOWN`] is the equivalent for the side plate.
struct Baselines {
    dpi: u32,
    scroll: u32,
    plate: u32,
    plate_label: Option<String>,
    /// When this device last told us something. `None` until it ever has.
    ///
    /// Exists to ORDER a push against a device scan. A scan reads the hardware at one instant and
    /// lands on the UI some hundreds of milliseconds later; a push that arrives inside that gap
    /// describes a newer reality than the scan does, and applying the scan's rows blind would
    /// revert it with nothing left to correct it until the next change. Whoever merges the two
    /// compares this against the moment the scan STARTED reading.
    at: Option<Instant>,
}
impl Default for Baselines {
    fn default() -> Self {
        Baselines { dpi: 0, scroll: 0, plate: PLATE_UNKNOWN, plate_label: None, at: None }
    }
}

/// The per-pid de-dup baselines, lazily created. `confirm` stays PURE — a std `HashMap` behind a
/// `Mutex`, no platform deps. The lock is taken poison-tolerantly at every site so a transient panic in
/// one consumer can't cascade-fail the de-dup for every device.
fn baselines() -> &'static Mutex<HashMap<u16, Baselines>> {
    static B: OnceLock<Mutex<HashMap<u16, Baselines>>> = OnceLock::new();
    B.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Run `f` against `pid`'s baselines, creating a default entry on first touch. The lock is released
/// when this returns — callers that also [`emit`] do so OUTSIDE it, so no consumer runs under the lock.
fn with_baseline<R>(pid: u16, f: impl FnOnce(&mut Baselines) -> R) -> R {
    let mut g = baselines().lock().unwrap_or_else(|p| p.into_inner());
    f(g.entry(pid).or_default())
}

/// Like [`with_baseline`], but also stamps the device as having just spoken. Every site that WRITES
/// a baseline goes through this; a site that only reads must not, or a read would forge freshness.
fn with_baseline_observed<R>(pid: u16, f: impl FnOnce(&mut Baselines) -> R) -> R {
    with_baseline(pid, |b| {
        b.at = Some(Instant::now());
        f(b)
    })
}

/// This device's last pushed DPI, but only if it arrived after `since`. `None` when the device has
/// said nothing, has never reported a DPI, or last spoke before `since`.
///
/// The one question a scan-merge needs answered: is there an observation strictly newer than the
/// hardware read I am about to apply? Because the device pushes on EVERY change, a later
/// observation is by construction fresher than the scan's value.
pub fn dpi_since(pid: u16, since: Instant) -> Option<u32> {
    let g = baselines().lock().unwrap_or_else(|p| p.into_inner());
    let b = g.get(&pid)?;
    (b.dpi > 0 && b.at.is_some_and(|t| t > since)).then_some(b.dpi)
}

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

/// DPI committed to `value` (was `prev`) on device `pid`, on the 100..30000 track.
pub fn dpi(pid: u16, value: u32, prev: Option<u32>) {
    with_baseline_observed(pid, |b| b.dpi = value); // baseline for THIS device's event de-dup
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

/// The sniper hold engaged (`engaged`, DPI dropped to `value`) or released (DPI restored to
/// `value`) on device `pid`. ALWAYS updates the pid's DPI baseline — even when the Sniper kind is
/// gated off downstream — so the device's own echo of the sniper write is absorbed by the ordinary
/// observed-event dedup and can never misattribute as a plain [`Kind::Dpi`] card (the in-game
/// "DPI changed!" spam this kind exists to kill). One `ident` for both edges: a hold is one card
/// that moves, never an on-card stacked on an off-card.
pub fn sniper(pid: u16, value: u32, prev: Option<u32>, engaged: bool) {
    with_baseline(pid, |b| b.dpi = value);
    emit(Confirmation {
        kind: Kind::Sniper,
        shape: Shape::Ranged {
            value: value as f64,
            min: 100.0,
            max: 30_000.0,
            unit: "DPI",
        },
        title: if engaged { "Sniper on" } else { "Sniper off" }.into(),
        ident: "sniper".into(),
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

/// A sensitivity / scroll stage committed to `value` on device `pid`, on a 0..`max` track.
pub fn scroll(pid: u16, value: u32, max: u32, prev: Option<u32>) {
    with_baseline_observed(pid, |b| b.scroll = value); // baseline for THIS device's event de-dup
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

/// Device `pid` reported its DPI is now `value` (e.g. you pressed its onboard DPI button). Cards it
/// only if it differs from the last DPI we confirmed FOR THAT DEVICE (host writes to it share that
/// baseline; a sibling mouse has its own). The `prev` of a first-ever observation is unknown, so it
/// reads as a bare new value rather than a false old→new.
pub fn observe_dpi(pid: u16, value: u32) {
    let prev = with_baseline(pid, |b| b.dpi);
    if value == prev {
        return;
    }
    dpi(pid, value, if prev == 0 { None } else { Some(prev) });
}

/// Device `pid` reported its scroll/sensitivity stage is now `stage` (its onboard stage button), on a
/// 1..`max` track. Same per-device de-dup contract as [`observe_dpi`].
pub fn observe_scroll(pid: u16, stage: u32, max: u32) {
    let prev = with_baseline(pid, |b| b.scroll);
    if stage == prev {
        return;
    }
    scroll(pid, stage, max, if prev == 0 { None } else { Some(prev) });
}

/// Device `pid` reported a swappable SIDE PLATE change: `id` is the raw hardware strap-code, `label`
/// its already-resolved human form (resolved DATA-side via the registry's `[side_plates]` map at the
/// decode site; `0`/detached resolves to "detached"). The mid-seat seating BOUNCE is debounced upstream
/// in `hidwatch`, so this only ever sees the settled code. It ALWAYS updates this device's readout (the
/// per-pid plate label the GUI polls, since the plate has no getter), but emits a confirmation CARD only
/// when the code BOTH differs from the last one we saw FOR THIS DEVICE (a double-send is swallowed) AND
/// is a real plate (`id != 0`): a detach is the silent "no plate" beat of a swap, not its own notice.
pub fn observe_side_plate(pid: u16, id: u32, label: &str) {
    // Swap this device's plate baseline AND refresh its readout atomically under one lock, so a same-
    // code re-report or a detach still keeps the readout honest. `prev` is this pid's last known.
    let prev = with_baseline_observed(pid, |b| {
        let prev = b.plate;
        b.plate = id;
        b.plate_label = Some(label.to_string());
        prev
    });
    if id == prev {
        return; // same plate as last known (a double-send) — readout current, no card
    }
    // A detach (id 0) updates the readout above but fires no card; a real plate cards once.
    if id != 0 {
        side_plate(label);
    }
}

// ── STATE SYNC: learn a value WITHOUT carding ────────────────────────────────────────────────────
// A wireless mouse RE-ANNOUNCES its whole settings state (dpi + scroll + side plate) in a tight burst
// when it wakes/reconnects. That is a sync, not user input — carding each one spams notifications the
// user never asked for. The decision "is this a sync" needs timing across kinds, which lives in the
// device layer (`hidwatch`); when it concludes a burst IS a sync it routes the values HERE instead of
// to the `observe_*` path. These mirror their `observe_*` twins but suppress the card: they update the
// de-dup baseline (and, for the plate, the GUI readout) so a LATER genuine change still cards from the
// right `prev`. `confirm` stays pure — no timing, no threads.

/// Learn device `pid`'s current DPI without carding (a wake/reconnect state sync). See module note above.
pub fn prime_dpi(pid: u16, value: u32) {
    with_baseline_observed(pid, |b| b.dpi = value);
}

/// Learn device `pid`'s current scroll/sensitivity stage without carding (state sync). See [`prime_dpi`].
pub fn prime_scroll(pid: u16, stage: u32) {
    with_baseline_observed(pid, |b| b.scroll = stage);
}

/// Learn device `pid`'s current side plate without carding (state sync): updates that device's de-dup
/// baseline AND its GUI readout label (the plate has no getter), exactly like [`observe_side_plate`]'s
/// readout path, but emits no card. See [`prime_dpi`].
pub fn prime_side_plate(pid: u16, id: u32, label: &str) {
    with_baseline_observed(pid, |b| {
        b.plate = id;
        b.plate_label = Some(label.to_string());
    });
}

/// The last side-plate label device `pid` pushed, if any has been observed since launch. Surfaced on
/// the DEVICE page per device (the plate is push-only, so this last-known value is the honest readout).
/// `None` until the first observation for that pid.
pub fn last_plate(pid: u16) -> Option<String> {
    let g = baselines().lock().unwrap_or_else(|p| p.into_inner());
    g.get(&pid).and_then(|b| b.plate_label.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    // The emission sink is process-global, so any test that drives an `observe` and reads the sink must
    // run alone. This lock serializes them within the lib-test binary. Acquired poison-tolerantly
    // (below): if a sibling test transiently panics under heavy parallel load it must not poison the
    // lock and cascade-fail the others — each test resets the pid baselines it touches up front, so
    // inheriting the guard after a poison is harmless.
    static TEST_LOCK: Mutex<()> = Mutex::new(());

    // Two distinct device product-ids to prove per-pid isolation. The actual values are arbitrary; what
    // matters is that they're different keys into the baseline map.
    const PID_A: u16 = 0x00A8; // Naga V2 Pro dongle (the plated mouse)
    const PID_B: u16 = 0x0084; // a second, independent mouse

    /// Reset `pid`'s de-dup baselines to "nothing observed yet" — equivalent to a fresh launch for that
    /// device. Removing the entry hands back a `Default` on next touch.
    fn reset_baseline(pid: u16) {
        let mut g = baselines().lock().unwrap_or_else(|p| p.into_inner());
        g.remove(&pid);
    }

    /// Drive `f` with a fresh plate baseline FOR `pid` + a private sink, and return the SIDE-PLATE
    /// confirmations it produced PLUS a snapshot of `last_plate(pid)` read UNDER the lock (other kinds a
    /// concurrent test might emit are filtered out). The plate snapshot is taken HERE, inside the
    /// critical section — not by the caller after the guard drops — so a sibling side-plate test can't
    /// race `pid`'s readout between this returning and the assertion. That post-lock read was the real
    /// cause of the rare parallel-run flake; asserting on this returned snapshot removes it.
    fn capture_side_plates(pid: u16, f: impl FnOnce()) -> (Vec<Confirmation>, Option<String>) {
        let _g = TEST_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        reset_baseline(pid);
        let (tx, rx) = mpsc::channel();
        set_sink(Some(tx));
        f();
        set_sink(None);
        let fired: Vec<Confirmation> = rx.try_iter().filter(|c| c.kind == Kind::SidePlate).collect();
        (fired, last_plate(pid))
    }

    #[test]
    fn side_plate_dedups_repeats() {
        // same plate reported twice (the device double-sends) → exactly ONE card.
        let (fired, plate) = capture_side_plates(PID_A, || {
            observe_side_plate(PID_A, 3, "12-button");
            observe_side_plate(PID_A, 3, "12-button");
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
        let (fired, plate) = capture_side_plates(PID_A, || {
            observe_side_plate(PID_A, 4, "6-button"); // currently seated (the "before")
            observe_side_plate(PID_A, 4, "6-button"); // mid-seat transient — same as last → no card
            observe_side_plate(PID_A, 3, "12-button"); // settled — differs → one card
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
        let (fired, plate) = capture_side_plates(PID_A, || observe_side_plate(PID_A, 0, "detached"));
        assert!(fired.is_empty(), "a detach must not card");
        assert_eq!(plate.as_deref(), Some("detached"), "but the readout still updates");
    }

    #[test]
    fn side_plate_swap_cards_only_the_real_plate_not_the_detach() {
        // a full swap: the old plate is seated, it detaches mid-swap (silent), then the new plate
        // seats (cards once). The detach in the middle updates the readout but adds no card.
        let (fired, plate) = capture_side_plates(PID_A, || {
            observe_side_plate(PID_A, 4, "6-button"); // seated before — cards
            observe_side_plate(PID_A, 0, "detached"); // mid-swap detach — silent, readout only
            observe_side_plate(PID_A, 3, "12-button"); // new plate seats — cards
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

    #[test]
    fn dpi_dedup_is_per_pid() {
        // THE CROSS-DEVICE FIX: two armed mice keep INDEPENDENT DPI baselines, so one mouse's value can
        // never suppress or misattribute the other's. Under the old PROCESS-GLOBAL baseline, after A set
        // it to 800, B's genuine change TO 800 (B was at 400) would read `== prev` and be silently
        // swallowed. Per-pid, each device de-dups only against itself.
        let _g = TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        reset_baseline(PID_A);
        reset_baseline(PID_B);
        let (tx, rx) = mpsc::channel();
        set_sink(Some(tx));
        observe_dpi(PID_B, 400); // B first sighting → cards (sets B's baseline to 400)
        observe_dpi(PID_A, 800); // A first sighting → cards (sets A's baseline to 800)
        observe_dpi(PID_B, 800); // B genuinely 400→800 → MUST card (global would swallow: == A's 800)
        observe_dpi(PID_B, 800); // B repeats 800 → SAME pid, same value → de-dups (no card)
        set_sink(None);

        let vals: Vec<f64> = rx
            .try_iter()
            .filter(|c| c.kind == Kind::Dpi)
            .filter_map(|c| match c.shape {
                Shape::Ranged { value, .. } => Some(value),
                _ => None,
            })
            .collect();
        // exactly three cards: B@400, A@800, B@800 — proving B's 800 was NOT suppressed by A's baseline,
        // and the trailing repeat added nothing (per-pid same-value de-dup still holds).
        assert_eq!(vals, vec![400.0, 800.0, 800.0], "per-pid baselines must not conflate across mice");
    }

    #[test]
    fn side_plate_readout_is_per_pid() {
        // Each device's plate readout is independent: a swap on A must not change B's last-known plate,
        // and a real plate on each cards from ITS OWN baseline.
        let _g = TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        reset_baseline(PID_A);
        reset_baseline(PID_B);
        let (tx, rx) = mpsc::channel();
        set_sink(Some(tx));
        observe_side_plate(PID_A, 3, "12-button"); // A → 12-button (cards)
        observe_side_plate(PID_B, 4, "6-button"); // B → 6-button (cards, NOT suppressed by A)
        set_sink(None);
        let cards = rx.try_iter().filter(|c| c.kind == Kind::SidePlate).count();
        assert_eq!(cards, 2, "each device's real plate cards independently");
        assert_eq!(last_plate(PID_A).as_deref(), Some("12-button"), "A keeps its own readout");
        assert_eq!(last_plate(PID_B).as_deref(), Some("6-button"), "B keeps its own readout");
    }
}
