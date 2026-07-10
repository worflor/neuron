//! Automatic device synthesis — the generalization of the hand-written `devices/*.toml`s.
//!
//! The two builtin defs were reverse-engineered by probing the `razer_report` getter space
//! and watching the device answer SUCCESS / UNSUPPORTED per command. That loop is mechanical,
//! so this module runs it automatically: probe an unknown Razer control pipe against the
//! UNIVERSAL COMMAND CATALOG (the union of the proven builtin specs — OpenRazer-derived,
//! hardware-verified on the Naga V2 Pro + BlackWidow Chroma V2), keep exactly the commands the
//! device says yes to, detect the lighting dialect from which lighting getters answer, measure
//! the link's real round-trip to pick `stream_wait_us`, and assemble a complete [`DeviceDef`].
//!
//! The synthesized def is then written to `devices/auto/razer-<pid>.toml` — from that moment
//! it is ORDINARY CONFIG, loaded by [`crate::registry::Registry::load`] like any other file,
//! editable and (by copying into `devices/`) promotable to a curated def that shadows it.
//! Discovery happens once per device; the file is the memo.
//!
//! What synthesis intentionally does NOT do:
//! * no writes — the probe is getter-only (ids ≥ 0x80 / documented read opcodes). Setters are
//!   emitted only when their PAIRED getter answered, mirroring how the builtins were proven.
//! * no capture-only commands — `set_scroll_stage` (wire-captured) and `[side_plates]`
//!   (push-report map) can't be discovered by asking; they stay per-device curated config.
//! * no transaction-id brute force — writes don't echo honesty (the Chroma V2's wrong-tx
//!   lighting writes ACK'd then no-op'd), so tx comes from the era heuristic below and is a
//!   one-line config fix when a board disagrees.

use crate::lighting::{LightingDef, Protocol};
use crate::registry::{CommandSpec, ControlInterface, DefOrigin, DeviceDef, Mode, Registry};
use crate::transport::{self, HidDeviceInfo, Transport};
use anyhow::{Context, Result};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// The razer_report bus signature — now OWNED by the dialect that defines the family
/// ([`crate::dialect`], `RazerDialect::claims`); re-exported here so the `neuron::synth::RAZER_VID`
/// / `RAZER_FEATURE_LEN` paths the app/CLI already resolve through keep compiling unchanged, and
/// synth's own probe filters below stay one edit from the signature's single home.
pub use crate::dialect::{RAZER_FEATURE_LEN, RAZER_VID};

/// The dialect registry: adoption below routes pipes to families by CLAIMING (the
/// generalization of the old hardcoded razer signature test). The trait itself needs no
/// import — its methods resolve directly on the registry's `dyn Dialect` handles.
use crate::dialect::dialects;

/// Evidence-carrying wrappers (DIALECT-RND "provenance as types", phase 1). A `Proven<T>` can
/// only be minted by the probe loop that OBSERVED the device answer; `Heuristic<T>` is era
/// inference. `emit_toml` derives its heuristic-warning comments MECHANICALLY from which fields
/// wear which wrapper — the file's honesty is a property of the types, not hand-maintained
/// prose. Phase 2 (future) pushes `Proven` outward through registry/write seams.
pub struct Proven<T>(T);
impl<T> Proven<T> {
    /// Private: only this module's probe path mints a `Proven` — downstream code cannot forge one
    /// (there is no public constructor). That privacy IS the compile-time honesty guarantee.
    fn mint(v: T) -> Self {
        Proven(v)
    }
    pub fn get(&self) -> &T {
        &self.0
    }
    pub fn into_inner(self) -> T {
        self.0
    }
}
/// Era-inference, freely constructible (`Heuristic(x)`) — the opposite of `Proven`: a value that
/// reached the def by reasoning about the device class, not by watching the device answer.
pub struct Heuristic<T>(pub T);

/// Probe transaction id. Getters ignore the tx field (proven live on both eras), so any
/// value works for the read-only probe; 0x1F matches `discover`.
const PROBE_TX: u8 = 0x1F;

/// A setter emitted alongside its proven getter (never probed — writes are not probes).
struct Setter {
    name: &'static str,
    class: u8,
    id: u8,
    size: u8,
}

/// One universal-catalog getter: probed read-only; if the device answers SUCCESS the getter
/// (with `args` baked in, exactly as the builtins bake them) and its paired setters join the
/// synthesized command map. Specs are byte-identical to the hand-verified builtin TOMLs.
struct CatalogEntry {
    name: &'static str,
    class: u8,
    id: u8,
    size: u8,
    args: &'static [u8],
    setters: &'static [Setter],
}

/// The universal command catalog — the union of the builtin defs' command maps.
const CATALOG: &[CatalogEntry] = &[
    CatalogEntry { name: "firmware_version", class: 0x00, id: 0x81, size: 0x02, args: &[], setters: &[] },
    CatalogEntry { name: "serial", class: 0x00, id: 0x82, size: 0x16, args: &[], setters: &[] },
    CatalogEntry { name: "device_mode", class: 0x00, id: 0x84, size: 0x02, args: &[], setters: &[] },
    CatalogEntry {
        name: "polling_rate", class: 0x00, id: 0x85, size: 0x01, args: &[],
        setters: &[Setter { name: "set_polling", class: 0x00, id: 0x05, size: 0x01 }],
    },
    CatalogEntry {
        name: "polling2", class: 0x00, id: 0xC0, size: 0x01, args: &[],
        setters: &[Setter { name: "set_polling2", class: 0x00, id: 0x40, size: 0x02 }],
    },
    CatalogEntry {
        name: "dpi", class: 0x04, id: 0x85, size: 0x07, args: &[],
        setters: &[Setter { name: "set_dpi", class: 0x04, id: 0x05, size: 0x07 }],
    },
    CatalogEntry {
        name: "dpi_stages", class: 0x04, id: 0x83, size: 0x26, args: &[],
        setters: &[Setter { name: "set_dpi_stages", class: 0x04, id: 0x06, size: 0x26 }],
    },
    CatalogEntry { name: "dpi_stages_active", class: 0x04, id: 0x86, size: 0x26, args: &[], setters: &[] },
    CatalogEntry { name: "battery_level", class: 0x07, id: 0x80, size: 0x02, args: &[], setters: &[] },
    CatalogEntry { name: "charging_status", class: 0x07, id: 0x84, size: 0x02, args: &[], setters: &[] },
    CatalogEntry { name: "storage_info", class: 0x06, id: 0x8E, size: 0x20, args: &[], setters: &[] },
    CatalogEntry { name: "storage_counts", class: 0x06, id: 0x80, size: 0x20, args: &[], setters: &[] },
    CatalogEntry { name: "storage_directory", class: 0x06, id: 0x8D, size: 0x20, args: &[], setters: &[] },
    // Matrix-era lighting getters (class 0x0F). brightness reads the VISIBLE region 0x04 —
    // the same region set_brightness writes (the read/write-region mismatch was a live bug).
    CatalogEntry {
        name: "brightness", class: 0x0F, id: 0x84, size: 0x03, args: &[0x00, 0x04],
        setters: &[Setter { name: "set_brightness", class: 0x0F, id: 0x04, size: 0x03 }],
    },
    CatalogEntry { name: "lighting_state", class: 0x0F, id: 0x82, size: 0x20, args: &[], setters: &[] },
    // FIRMWARE GAME MODE (legacy-era, class 0x03) — the keyboard's own FN+F10 Win-key kill
    // (GAME_LED state). CONFIRMED on the BlackWidow Chroma V2 (2026-07-07). The getter probe is
    // READ-ONLY (args [varstore, GAME_LED]), and ONLY legacy-era boards answer class 0x03, so a
    // matrix-era mouse never grows a fake game-mode from this probe. Its paired setter joins the map
    // when the getter answers. Names are UNIQUE (unlike the two eras' shared "lighting_state"), so
    // the catalog's first-answering-wins name-collision rule does NOT apply here.
    CatalogEntry {
        name: "game_mode", class: 0x03, id: 0x80, size: 0x03, args: &[0x00, 0x08],
        setters: &[Setter { name: "set_game_mode", class: 0x03, id: 0x00, size: 0x03 }],
    },
    // Legacy-era lighting getters (class 0x03). `lighting_state` intentionally reuses the
    // matrix entry's name — first-answering dialect wins the slot (matrix is probed first).
    CatalogEntry { name: "lighting_state", class: 0x03, id: 0x88, size: 0x06, args: &[], setters: &[] },
    CatalogEntry { name: "lighting_caps", class: 0x03, id: 0x89, size: 0x07, args: &[], setters: &[] },
];

/// OpenRazer's per-receiver SET→GET wait for "new mouse receiver" wireless dongles (µs).
const WIRELESS_STREAM_WAIT_US: u64 = 31_000;
/// Round-trips at/above this are a slow (wireless) link that needs the stream wait.
const WIRELESS_ROUNDTRIP_MS: u64 = 15;

/// `stream_wait_us` for the synthesized def. A wireless link overruns without pacing —
/// streamed lighting writes outrun the host→dongle→device round-trip and DROP frames (the
/// FLICKER) — so it gets OpenRazer's receiver wait. Wirelessness is detected two ways,
/// either sufficing, because the LIVE-VERIFIED failure mode of latency alone is a dongle
/// that ACKs getters in ~6ms yet still needs the write pacing (the Naga's receiver did
/// exactly that):
///   * a slow measured round-trip (the direct evidence), or
///   * a battery (`wireless_capable`) — a battery-bearing board is built to run wireless,
///     and the wait on a wired link only lowers the stream fps ceiling, never corrupts;
///     flicker-by-default on a real dongle would be the dishonest failure mode.
pub fn stream_wait_for(roundtrip_ms: u64, wireless_capable: bool) -> u64 {
    if roundtrip_ms >= WIRELESS_ROUNDTRIP_MS || wireless_capable {
        WIRELESS_STREAM_WAIT_US
    } else {
        0
    }
}

/// Per-command probe window (2ms polls): ~320ms covers the slowest awake wireless round-trip.
const PROBE_POLLS: usize = 160;
/// Qualify window: the FIRST command to the device also has to wake a parked wireless radio,
/// which takes host traffic on the order of seconds — give it ~2s of re-armed polling. (A
/// DEEP-asleep mouse only wakes on user input; no window fixes that — the retry paths do.)
const QUALIFY_POLLS: usize = 1000;

/// Send one getter and poll (2ms grain, up to `max_polls`) for its echoed reply. Returns the
/// args and how long the round-trip took. Finer-grained than `discover::exec` because the
/// measured latency FEEDS `stream_wait_us` — 8ms polling would read every wired board as ~8ms.
fn timed_exec(
    t: &dyn Transport,
    class: u8,
    id: u8,
    size: u8,
    args: &[u8],
    max_polls: usize,
) -> Option<([u8; 80], Duration)> {
    use crate::protocol::{reply_status, Report, Status, BUF_LEN};

    // Hold the pipe's wire lock for this ONE request/reply conversation only — not the whole probe
    // sweep. A sweep touches dozens of commands; locking per-pair (not per-sweep) is what lets a
    // 30fps lighting writer interleave between pairs instead of starving for the sweep's duration.
    let wire = t.wire_lock();
    let _wire = wire.as_ref().map(|w| w.acquire());

    let mut req = Report::command(PROBE_TX, class, id, size);
    for (i, b) in args.iter().enumerate() {
        if i < req.args.len() {
            req.args[i] = *b;
        }
    }
    let out = req.to_buf();
    let start = Instant::now();
    t.set_feature(&out).ok()?;
    for i in 0..max_polls {
        std::thread::sleep(Duration::from_millis(2));
        let mut b = [0u8; BUF_LEN];
        if t.get_feature(&mut b).is_ok() {
            // Shared echo filter (dialect seam) — same class/id echo the busy-poll loops use; only
            // the 2ms cadence differs (it IS the stream_wait_us calibration instrument).
            if let Some(status) = reply_status(&b, class, id) {
                match status {
                    Status::Success => return Some((Report::from_buf(&b).args, start.elapsed())),
                    Status::Fail | Status::Unsupported => return None,
                    _ => {}
                }
            }
        }
        if i % 48 == 47 {
            let _ = t.set_feature(&out); // re-arm a busy/slow/waking wireless link
        }
    }
    None
}

/// Everything synthesis needs to know about the pipe besides the transport itself.
pub struct SynthCtx {
    pub vid: u16,
    pub pid: u16,
    pub usage_page: u16,
    pub usage: u16,
    /// The pipe's `FeatureReportByteLength`. Razer synthesis ignores it (its control interface is the
    /// fixed [`RAZER_FEATURE_LEN`]); it is carried for dialects whose def wants the enumerated value —
    /// e.g. hidpp stores it as `control_interface.feature_report_len` even though HID++ MATCHING rides
    /// `claims()`, not the feature length (HID++ uses output/input reports, not feature reports).
    pub feature_len: u16,
    /// USB product string (may be empty) — becomes the def's honest name.
    pub product: String,
}

impl SynthCtx {
    pub fn from_info(i: &HidDeviceInfo) -> Self {
        SynthCtx {
            vid: i.vid,
            pid: i.pid,
            usage_page: i.usage_page,
            usage: i.usage,
            feature_len: i.feature_len,
            product: i.product.clone(),
        }
    }
}

/// A synthesized device: the def (plain registry data, what the registry consumes) PLUS the typed
/// record of HOW it was built. The wrapped fields DUPLICATE values that also live in `def` — that
/// duplication is the point: `def` is data, the wrappers are EVIDENCE. `emit_toml` reads the
/// wrappers (not the def) to decide which honesty comments the file carries, so the file's
/// warnings track the types automatically (DIALECT-RND "provenance as types", phase 1).
pub struct Synthesis {
    pub def: DeviceDef,
    /// Device serial (when the serial getter answered) — file-comment provenance.
    pub serial: String,
    /// Median measured command round-trip (ms) — the basis of `stream_wait_us`.
    pub roundtrip_ms: u64,
    /// The transaction id — ALWAYS era-inference (writes don't echo honesty, so tx can't be
    /// probe-proven), hence `Heuristic`. Duplicates `def.transaction_id`.
    pub tx: Heuristic<u8>,
    /// Lighting matrix dimensions — a GUESS a getter can't reveal. `None` when the def has no
    /// lighting block (nothing to warn about). Duplicates `def.lighting`'s rows/cols when `Some`.
    pub dims: Option<Heuristic<(u8, u8)>>,
    /// The streamed-write pacing — link/battery inference, hence `Heuristic`. Duplicates
    /// `def.stream_wait_us`.
    pub stream_wait: Heuristic<u64>,
    /// The catalog command names the device actually answered SUCCESS for (plus the setters those
    /// answers unlock) — minted inside the probe loop, so a `Proven` here is by construction the
    /// OBSERVED result. Equals `def.commands`' keys.
    pub proven_commands: Proven<Vec<String>>,
}

impl Synthesis {
    /// The cross-dialect mint seam (pub(crate)): a Dialect's probe hands over WHAT IT OBSERVED
    /// (the command names whose getters answered) and WHAT IT INFERRED, and this constructor —
    /// not the caller — mints the Proven wrapper. Keeps Proven un-forgeable outside synth while
    /// letting sibling dialect modules synthesize: the evidence contract travels through the
    /// signature, not through access to the wrapper's internals.
    ///
    /// Razer's own [`synthesize`] does NOT route through here — it lives in this module and mints
    /// its Proven directly. This seam exists for the OTHER dialects (hidpp, …) whose probe code
    /// lives in a sibling module and therefore cannot reach [`Proven::mint`]; the signature is the
    /// contract they satisfy in lieu of that access.
    pub(crate) fn from_probe(
        def: DeviceDef,
        answered_commands: Vec<String>,
        tx: Heuristic<u8>,
        dims: Option<Heuristic<(u8, u8)>>,
        stream_wait: Heuristic<u64>,
        serial: String,
        roundtrip_ms: u64,
    ) -> Self {
        // A probe may not claim evidence for a command it did not EMIT: every answered name must be
        // a key in the def's command map, or the mint would be laundering an unobserved name into
        // `Proven`. debug_assert (not a hard error) because a release build trusts its own dialects
        // — this catches a dialect wiring its answered-set and its command map out of agreement.
        debug_assert!(
            answered_commands.iter().all(|c| def.commands.contains_key(c)),
            "from_probe: answered_commands must be a subset of def.commands keys"
        );
        Synthesis {
            def,
            serial,
            roundtrip_ms,
            tx,
            dims,
            stream_wait,
            proven_commands: Proven::mint(answered_commands),
        }
    }
}

fn cmd(class: u8, id: u8, size: u8, args: &[u8]) -> CommandSpec {
    CommandSpec {
        class,
        id,
        size,
        args: args.to_vec(),
        transaction_id: None,
    }
}

fn cmd_tx(class: u8, id: u8, size: u8, args: &[u8], tx: u8) -> CommandSpec {
    CommandSpec {
        transaction_id: Some(tx),
        ..cmd(class, id, size, args)
    }
}

/// The matrix (extended, class 0x0F) lighting block — Naga-proven values. `custom_id` MUST be
/// 0x08 on this era (0x05 is a real native effect there — REACTIVE on the Naga — and using it
/// as the display id painted-then-un-painted every streamed frame).
fn matrix_lighting(rows: u8, cols: u8, brightness_proven: bool) -> LightingDef {
    LightingDef {
        protocol: Protocol::Matrix,
        rows,
        cols,
        varstore: 0x00,
        led_id: 0x04,
        effects: [
            ("off", 0x00u8),
            ("static", 0x01),
            ("breathing", 0x02),
            ("wave", 0x03),
            ("spectrum", 0x04),
            ("reactive", 0x05),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect(),
        custom_id: 0x08,
        effect: cmd(0x0F, 0x02, 0x0C, &[0x00, 0x04]),
        custom_frame: Some(cmd(0x0F, 0x03, 0x40, &[0x00, 0x04])),
        // The matrix dialect HAS a brightness getter (0x0F/0x84 — the same probe that gates the
        // top-level `set_brightness` command), so the block's brightness rides that SAME evidence:
        // emit it only when the getter answered. `lighting.brightness` is now a WRITE path
        // (`DeviceDef::supports(SetBrightness)` reads it), so an unanswered getter must mean NO
        // brightness write anywhere in the def — leave it `None` and `supports(SetBrightness)`
        // honestly reads false rather than promising an unproven write.
        brightness: brightness_proven.then(|| cmd(0x0F, 0x04, 0x03, &[0x00, 0x04])),
    }
}

/// The legacy (standard, class 0x03) lighting block — Chroma-V2-proven values, including the
/// split-brain tx: getters ride the device default while EFFECT and CUSTOM-FRAME writes need
/// 0x3F (at the default they ACK and silently no-op). No brightness — see the field comment.
fn legacy_lighting(rows: u8, cols: u8) -> LightingDef {
    LightingDef {
        protocol: Protocol::Legacy,
        rows,
        cols,
        varstore: 0x00,
        led_id: 0x00,
        effects: [
            ("off", 0x00u8),
            ("wave", 0x01),
            ("reactive", 0x02),
            ("breathing", 0x03),
            ("spectrum", 0x04),
            ("static", 0x06),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect(),
        custom_id: 0x05,
        effect: cmd_tx(0x03, 0x0A, 0x08, &[], 0x3F),
        custom_frame: Some(cmd_tx(0x03, 0x0B, 0x46, &[0xFF], 0x3F)),
        // No brightness: the legacy dialect has NO brightness getter to prove a setter against
        // (the curated BlackWidow's 0x03/0x03 write was verified on that physical board —
        // per-device evidence a blind probe cannot produce). Now that `lighting.brightness`
        // GRANTS a write capability, baking the standard 0x03/0x03 spec here would put unproven
        // bytes on the wire for every legacy board synthesis touches. A user who KNOWS their
        // legacy board takes the standard write adds `[lighting.brightness]` to the auto file by
        // hand — it is config.
        brightness: None,
    }
}

/// Probe one Razer control pipe and synthesize its complete [`DeviceDef`]. Read-only.
/// `None` when the pipe never answers the firmware getter (not a talking razer_report pipe —
/// e.g. a secondary collection that enumerates with the right shape but stays mute).
pub fn synthesize(t: &dyn Transport, ctx: &SynthCtx) -> Option<Synthesis> {
    // Wake + qualify: the first command to a sleeping wireless device can take an outlier
    // round-trip (it also has to wake the radio — the long QUALIFY window), so it qualifies
    // the pipe but doesn't count toward the latency sample.
    timed_exec(t, 0x00, 0x81, 0x02, &[], QUALIFY_POLLS)?;

    // Measure the link: median of three firmware round-trips.
    let mut samples: Vec<u64> = (0..3)
        .filter_map(|_| {
            timed_exec(t, 0x00, 0x81, 0x02, &[], PROBE_POLLS).map(|(_, d)| d.as_millis() as u64)
        })
        .collect();
    samples.sort_unstable();
    let roundtrip_ms = samples.get(samples.len() / 2).copied().unwrap_or(0);

    // Catalog probe: keep what answers. First-wins on name collisions (the two eras' state
    // getters share "lighting_state"; matrix precedes legacy in the catalog).
    let mut commands: BTreeMap<String, CommandSpec> = BTreeMap::new();
    let mut serial = String::new();
    let mut matrix = false;
    let mut legacy = false;
    for e in CATALOG {
        let Some((args, _)) = timed_exec(t, e.class, e.id, e.size, e.args, PROBE_POLLS) else {
            continue;
        };
        if e.name == "serial" {
            serial = args
                .iter()
                .take_while(|&&b| (0x20..0x7F).contains(&b))
                .map(|&b| b as char)
                .collect();
        }
        match (e.class, e.id) {
            (0x0F, _) => matrix = true,
            (0x03, _) => legacy = true,
            _ => {}
        }
        commands
            .entry(e.name.to_string())
            .or_insert_with(|| cmd(e.class, e.id, e.size, e.args));
        for s in e.setters {
            commands
                .entry(s.name.to_string())
                .or_insert_with(|| cmd(s.class, s.id, s.size, &[]));
        }
    }

    // Lighting dialect: matrix getters answering ⇒ extended matrix; else legacy getters ⇒
    // standard/legacy. Matrix wins when both answer (never observed; 0x0F supersedes 0x03).
    // rows×cols is the one thing a getter can't reveal — HEURISTIC (a DPI class = a mouse),
    // flagged loudly in the emitted file. Wrong dims degrade custom frames (partial paint),
    // never native effects, which are whole-device.
    let has_dpi = commands.contains_key("dpi");
    let lighting = if matrix {
        let (rows, cols) = if has_dpi { (1, 2) } else { (6, 22) };
        // "brightness" is the matrix getter's registry name — inserted above only when 0x0F/0x84
        // answered — so it is the exact probe evidence the block's brightness write must ride.
        Some(matrix_lighting(rows, cols, commands.contains_key("brightness")))
    } else if legacy {
        Some(legacy_lighting(6, 22))
    } else {
        None
    };

    // Era tx heuristic: matrix-era boards take 0x1F device-wide (Naga-proven); legacy-only
    // boards default 0xFF with the 0x3F lighting override baked into the lighting block
    // (Chroma-V2-proven). Getters ignore tx either way — only writes can disagree, and the
    // emitted file says exactly which knob to turn.
    let transaction_id = if matrix || !legacy { 0x1F } else { 0xFF };

    // The name mirrors the emitted file, which is the source of truth: a TOML basic string can't
    // carry control bytes, so `emit_toml` maps them to spaces — strip them here too (via the same
    // control→space rule) so the in-memory def equals what a reload of its own file would yield.
    // Quotes/backslashes are KEPT (they round-trip losslessly through `toml_escape`).
    let name = if ctx.product.trim().is_empty() {
        format!("Razer device {:04x} (auto)", ctx.pid)
    } else {
        comment_safe(ctx.product.trim())
    };
    let wireless_capable = commands.contains_key("battery_level");
    let stream_wait_us = stream_wait_for(roundtrip_ms, wireless_capable);
    // The evidence twins, captured at the SAME points the def's data is computed: dims mirror the
    // lighting block's guessed rows/cols (None when there's no block — nothing to warn about),
    // proven_commands mirror the answered command map. Both DUPLICATE what lands in the def; the
    // duplication is deliberate (def = data, wrappers = provenance) and `emit_toml` reads these.
    let dims = lighting.as_ref().map(|l| Heuristic((l.rows, l.cols)));
    let proven_commands = Proven::mint(commands.keys().cloned().collect());
    let def = DeviceDef {
        name,
        codename: format!("auto-{:04x}", ctx.pid),
        // Synthesis only ever probes razer_report pipes (RAZER_VID + 91-byte feature report), so
        // an auto def is razer by construction — matches the serde default a reload would apply.
        dialect: "razer".into(),
        // A synthesized def IS auto by construction — it lands in devices/auto/ and a reload
        // stamps it Auto anyway; setting it here keeps the in-memory def equal to a reloaded one
        // (so the first-light heal, which gates on origin == Auto, works on the fresh synth too).
        origin: DefOrigin::Auto,
        vendor_id: ctx.vid,
        transaction_id,
        stream_wait_us,
        modes: vec![Mode {
            name: "default".into(),
            product_id: ctx.pid,
        }],
        control_interface: ControlInterface {
            usage_page: ctx.usage_page,
            usage: ctx.usage,
            feature_report_len: RAZER_FEATURE_LEN,
        },
        commands,
        lighting,
        side_plates: None,
        // Synthesis never probes push-only report vocabularies (no HID reader in this pass) —
        // an auto def carries no `[events]` block; hidwatch's collection-shape arming is unaffected.
        events: None,
    };
    Some(Synthesis {
        def,
        serial,
        roundtrip_ms,
        tx: Heuristic(transaction_id),
        dims,
        stream_wait: Heuristic(stream_wait_us),
        proven_commands,
    })
}

/// Escape a string for a TOML basic-string value (`"..."`). Backslash and quote get their
/// TOML escapes; every ASCII control byte (`c < 0x20` or DEL) becomes a single space.
///
/// A USB product string is attacker-adjacent junk — a device is free to report a name carrying
/// a quote, a backslash, or raw control bytes, and interpolating that raw into `name = "{}"`
/// emits INVALID TOML. That is the worst failure this module has: [`Registry::load`] silently
/// skips the unparseable file, yet [`adopt_unknown`] sees `auto_def_path().exists()` and refuses
/// to clobber it — so the device is PERMANENTLY un-adopted until the user hand-edits the file.
/// A control byte in a name is garbage regardless, so a space keeps the file readable rather
/// than round-tripping junk; the FILE is the source of truth and must stay valid.
fn toml_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            c if (c as u32) < 0x20 || c == '\u{7F}' => out.push(' '),
            c => out.push(c),
        }
    }
    out
}

/// Make a string safe to interpolate INTO a `# ...` header comment line. A `\r`/`\n` here would
/// break out of the comment and turn the remainder of the product string (or serial) into stray
/// top-level TOML — corrupting the file exactly like an unescaped value does. Every control byte
/// collapses to a space (same reasoning as [`toml_escape`]: it stays a comment, stays readable).
fn comment_safe(s: &str) -> String {
    s.chars()
        .map(|c| if (c as u32) < 0x20 || c == '\u{7F}' { ' ' } else { c })
        .collect()
}

fn emit_cmd(out: &mut String, table: &str, c: &CommandSpec) {
    out.push_str(&format!("[{table}]\nclass = 0x{:02X}\nid = 0x{:02X}\nsize = 0x{:02X}\n", c.class, c.id, c.size));
    if !c.args.is_empty() {
        let args: Vec<String> = c.args.iter().map(|b| format!("0x{b:02X}")).collect();
        out.push_str(&format!("args = [{}]\n", args.join(", ")));
    }
    if let Some(tx) = c.transaction_id {
        out.push_str(&format!("transaction_id = 0x{tx:02X}\n"));
    }
    out.push('\n');
}

/// The provenance NOTES that head an emitted auto file — the human "how each field was decided"
/// block that rides above the data. Split out from the file BODY so ONE emitter ([`emit`]) serves
/// both writers without duplicating the file shape: [`emit_toml`] fills these from the `Synthesis`
/// evidence wrappers (era-inference caveats), while [`heal_auto_tx`] fills the SAME struct with a
/// different `transaction_id` line — one that says the tx was hardware-proven, not guessed.
struct EmitNotes {
    /// Probed unit serial (empty ⇒ the serial comment line is omitted). A heal has no re-probe,
    /// so it passes empty — the tx line carries the proof instead.
    serial: String,
    /// Measured command round-trip (ms) for the header's `~Nms -> stream_wait_us` line.
    roundtrip_ms: u64,
    /// The "HEURISTIC fields worth checking" caveat lines, in order — each a full `#   - …`
    /// comment (already control-safe). The `transaction_id` line is the one the heal rewrites.
    heuristic_lines: Vec<String>,
}

/// Serialize a synthesized def to the same commented-TOML shape as the curated files. The
/// output parses back into an identical [`DeviceDef`] (round-trip tested) — from here on
/// it's plain config the user owns.
pub fn emit_toml(s: &Synthesis) -> String {
    let d = &s.def;
    // The warning block is DERIVED from which synthesis fields wear a wrapper, not hand-written:
    // one Heuristic → one caveat line, in the original order. `tx` and `stream_wait` are ALWAYS
    // heuristic, so their lines always appear; `dims` exists (and can only mislead) only when
    // there's a lighting block; the brightness caveat rides the same lighting-block condition
    // (whether a brightness WRITE was actually proven lives in `proven_commands`/`def.lighting.
    // brightness` and gates the [lighting.brightness] TABLE below — this line documents the rule).
    let mut heuristic_lines: Vec<String> = Vec::new();
    // tx: Heuristic<u8> — always present.
    heuristic_lines
        .push("#   - transaction_id: era-derived, not probe-proven (writes don't echo honesty).".into());
    if s.dims.is_some() {
        // dims: Some only when there's a lighting block.
        heuristic_lines
            .push("#   - [lighting] rows/cols: a GUESS — native effects don't need it, custom frames do.".into());
    }
    if d.lighting.is_some() {
        heuristic_lines.push(
            "#   - [lighting.brightness]: emitted ONLY when the probe proved its getter. Legacy-dialect\n\
             #     defs omit it by design (no brightness getter exists to prove the write against) —\n\
             #     add [lighting.brightness] by hand if the board is known to take the standard write.".into(),
        );
    }
    // stream_wait: Heuristic<u64> — always present.
    heuristic_lines.push(
        "#   - stream_wait_us: battery-bearing boards get the wireless write pacing even on a\n\
         #     fast link (a dongle ACKs getters quickly yet still drops streamed frames without\n\
         #     it); set it to 0 for a permanently-wired board to raise the stream fps ceiling.".into(),
    );
    emit(
        d,
        &EmitNotes {
            serial: s.serial.clone(),
            roundtrip_ms: s.roundtrip_ms,
            heuristic_lines,
        },
    )
}

/// The shared file BODY: render `def` + `notes` to the commented-TOML shape. Both [`emit_toml`]
/// (fresh synthesis) and [`heal_auto_tx`] (tx rewrite) route through here, so a healed file is
/// byte-shaped identically to a synthesized one — only the notes differ. Producing the file from
/// the def means the healed tx (already set on `def`) appears in the `transaction_id = …` line.
fn emit(d: &DeviceDef, notes: &EmitNotes) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "# Device definition — {} (AUTO-SYNTHESIZED by probing the device's getter space\n\
         # through its wire dialect; every command below answered on the real hardware).\n",
        comment_safe(&d.name)
    ));
    if !notes.serial.is_empty() {
        out.push_str(&format!("# Probed unit serial: {}\n", comment_safe(&notes.serial)));
    }
    out.push_str(&format!(
        "# Measured command round-trip: ~{}ms -> stream_wait_us = {}.\n\
         # This file is CONFIG, not discovery: edit freely (it is never overwritten), or copy\n\
         # it into devices/ to promote it to a curated def that shadows this one.\n\
         # HEURISTIC fields worth checking on a new board:\n",
        notes.roundtrip_ms, d.stream_wait_us
    ));
    for line in &notes.heuristic_lines {
        out.push_str(line);
        out.push('\n');
    }
    out.push('\n');
    out.push_str(&format!("name = \"{}\"\n", toml_escape(&d.name)));
    out.push_str(&format!("codename = \"{}\"\n", toml_escape(&d.codename)));
    // `dialect` serde-defaults to "razer" on load, so razer auto files OMIT it (zero on-disk churn —
    // the generalization renames nothing and rewrites no bytes for the existing family). A non-razer
    // def MUST emit it: without the tag a reload would default it to "razer" and route the device's
    // bytes through the wrong exec loop. ("razer" is the serde default in `registry::default_dialect`.)
    if d.dialect != "razer" {
        out.push_str(&format!("dialect = \"{}\"\n", toml_escape(&d.dialect)));
    }
    out.push_str(&format!("vendor_id = 0x{:04X}\n", d.vendor_id));
    out.push_str(&format!("transaction_id = 0x{:02X}\n", d.transaction_id));
    if d.stream_wait_us > 0 {
        out.push_str(&format!("stream_wait_us = {}\n", d.stream_wait_us));
    }
    out.push('\n');
    for m in &d.modes {
        out.push_str(&format!(
            "[[modes]]\nname = \"{}\"\nproduct_id = 0x{:04X}\n\n",
            toml_escape(&m.name), m.product_id
        ));
    }
    out.push_str(&format!(
        "[control_interface]\nusage_page = 0x{:04X}\nusage = 0x{:04X}\nfeature_report_len = {}\n\n",
        d.control_interface.usage_page, d.control_interface.usage, d.control_interface.feature_report_len
    ));
    if d.commands.is_empty() {
        // `commands` has no `#[serde(default)]` (a load-time typo'd/truncated table should fail
        // loudly, not silently deserialize to empty) — so a genuinely EMPTY map (e.g. razer-audio's
        // synthesized def, which probes none) still needs the key to be PRESENT on disk, exactly the
        // convention the curated Seiren TOML used by hand before it was deleted.
        out.push_str("# No commands known — an honest empty table, not invented opcodes.\n[commands]\n\n");
    }
    for (name, c) in &d.commands {
        emit_cmd(&mut out, &format!("commands.{name}"), c);
    }
    if let Some(l) = &d.lighting {
        let proto = match l.protocol {
            Protocol::Matrix => "matrix",
            Protocol::Legacy => "legacy",
        };
        out.push_str(&format!(
            "[lighting]\nprotocol = \"{proto}\"\nrows = {}\ncols = {}   # HEURISTIC guess — refine for full custom-frame coverage\nvarstore = 0x{:02X}\nled_id = 0x{:02X}\ncustom_id = 0x{:02X}\n\n",
            l.rows, l.cols, l.varstore, l.led_id, l.custom_id
        ));
        out.push_str("[lighting.effects]\n");
        for (k, v) in &l.effects {
            out.push_str(&format!("{k} = 0x{v:02X}\n"));
        }
        out.push('\n');
        emit_cmd(&mut out, "lighting.effect", &l.effect);
        if let Some(cf) = &l.custom_frame {
            emit_cmd(&mut out, "lighting.custom_frame", cf);
        }
        if let Some(b) = &l.brightness {
            emit_cmd(&mut out, "lighting.brightness", b);
        }
    }
    out
}

/// Rewrite an AUTO def's file with a HARDWARE-VERIFIED transaction id (the first-light heal's
/// result). The tx line stops being heuristic — the emitted comment says exactly how it was
/// proven. Refuses non-auto paths by construction (only [`auto_def_path`] is ever written), so a
/// curated/builtin def can never be clobbered by this path even if a caller passed one in.
///
/// The notes carry the SAME caveats [`emit_toml`] derives EXCEPT the tx line, which now records
/// the proof. Serial + measured round-trip aren't recoverable from the def alone (they were probe
/// facts, not stored fields), so the header re-emits them empty/0 — the honest cost of healing
/// from the loaded def rather than a fresh probe; the tx proof is the line that matters here.
pub fn heal_auto_tx(dialect_id: &str, pid: u16, def: &DeviceDef, verified_tx: u8) -> Result<()> {
    let mut healed = def.clone();
    healed.transaction_id = verified_tx;
    // Rebuild the caveat block: every heuristic line EXCEPT tx persists (dims + brightness ride the
    // lighting block exactly as emit_toml gates them; stream_wait is always present); the tx line
    // now states hardware proof instead of era-inference.
    let mut heuristic_lines: Vec<String> = Vec::new();
    heuristic_lines.push(format!(
        "#   - transaction_id: HARDWARE-VERIFIED by first-light heal (custom frame landed + read back at 0x{verified_tx:02X})."
    ));
    if healed.lighting.is_some() {
        heuristic_lines
            .push("#   - [lighting] rows/cols: a GUESS — native effects don't need it, custom frames do.".into());
        heuristic_lines.push(
            "#   - [lighting.brightness]: emitted ONLY when the probe proved its getter. Legacy-dialect\n\
             #     defs omit it by design (no brightness getter exists to prove the write against) —\n\
             #     add [lighting.brightness] by hand if the board is known to take the standard write.".into(),
        );
    }
    heuristic_lines.push(
        "#   - stream_wait_us: battery-bearing boards get the wireless write pacing even on a\n\
         #     fast link (a dongle ACKs getters quickly yet still drops streamed frames without\n\
         #     it); set it to 0 for a permanently-wired board to raise the stream fps ceiling.".into(),
    );
    let text = emit(
        &healed,
        &EmitNotes {
            serial: String::new(),
            roundtrip_ms: 0,
            heuristic_lines,
        },
    );
    // Only ever the auto path — the file name is built from the dialect id + pid, exactly where a
    // reload expects it. create_dir_all mirrors adopt_filtered (the dir already exists since the
    // def was loaded from it, but a mid-run delete must not turn the heal into a panic).
    let file = auto_def_path(dialect_id, pid);
    std::fs::create_dir_all(file.parent().expect("auto path has a parent"))
        .and_then(|()| std::fs::write(&file, text))
        .with_context(|| format!("healing {} failed", file.display()))?;
    Ok(())
}

/// The collision-free adoption identity: a pid is only unique WITHIN a wire family (two
/// families can share one — the reason auto files are named <dialect>-<pid>). Everything that
/// tracks adoption lifecycle (worker scope, retry ledger, inflight rows) keys on this pair,
/// never bare pid. Dialect ids are &'static str from the registry, so the pair is Copy.
pub type AdoptKey = (&'static str, u16);

/// The adoption key for an enumerated pipe, when some family claims it.
pub fn adopt_key(info: &HidDeviceInfo) -> Option<AdoptKey> {
    crate::dialect::claimed_by(info).map(|d| (d.id(), info.pid))
}

/// One device adopted by [`adopt_unknown`].
pub struct Adopted {
    pub pid: u16,
    /// The wire family that claimed and synthesized this pipe (the `dialect` id half of its
    /// [`AdoptKey`]). Callers can report the family alongside the pid; the pid alone is ambiguous
    /// across families, so this is the disambiguator when two share one.
    pub dialect: &'static str,
    pub name: String,
    pub path: PathBuf,
}

/// The outcome of one [`adopt_unknown`] pass. Per-device problems land in `skipped` (with the
/// reason) instead of aborting the pass — one broken file or unwritable path must never stop a
/// DIFFERENT new device from being adopted.
#[derive(Default)]
pub struct Adoption {
    pub adopted: Vec<Adopted>,
    pub skipped: Vec<String>,
}

/// Where an auto-synthesized def lives (in the run root, like every other config path). Keyed by
/// the synthesizing DIALECT's id so two families that share a pid can't collide on one filename:
/// `devices/auto/<dialect_id>-<pid>.toml`. Razer's id is literally "razer", so every existing
/// auto file name (`razer-<pid>.toml`) is IDENTICAL — the generalization renames nothing on disk.
pub fn auto_def_path(dialect_id: &str, pid: u16) -> PathBuf {
    crate::runroot::run_root()
        .join("devices")
        .join("auto")
        .join(format!("{dialect_id}-{pid:04x}.toml"))
}

/// Find every connected Razer `razer_report` pipe the registry does NOT recognize, synthesize
/// a def for it, and write `devices/auto/razer-<pid>.toml`. Existing files are never
/// overwritten (they are the user's config now — a present-but-unloadable one is reported in
/// `skipped` instead). Per-device failures skip that device only; `Err` is reserved for HID
/// enumeration itself failing. Reload the registry to pick the new files up.
pub fn adopt_unknown(reg: &Registry) -> Result<Adoption> {
    adopt_filtered(reg, None)
}

/// Like [`adopt_unknown`], but adopt ONLY the given [`AdoptKey`]s — any other unknown pipe seen in
/// enumeration is left untouched. The app's background workers use this: a worker spawned for
/// one key must not also probe a DIFFERENT key that another worker is already adopting, or the
/// two race the `auto_def_path().exists()` check and double-write the same file. Constraining
/// each worker to its own keys is the single-writer half of the auto-file ownership rule (the
/// single-probe half is the caller's spawn guard). Keying on `(dialect, pid)` — not bare pid —
/// is what keeps that scope tight when two families share a pid: worker A's key never matches
/// worker B's same-pid pipe, so the two can't overlap on the other family's control pipe.
pub fn adopt_keys(reg: &Registry, keys: &[AdoptKey]) -> Result<Adoption> {
    adopt_filtered(reg, Some(keys))
}

/// Shared adoption pass. `only = None` adopts every unrecognized claimed pipe; `only = Some(keys)`
/// restricts the pass to those [`AdoptKey`]s (skipping any enumerated pipe whose key isn't in the
/// slice) so concurrent callers never overlap on a family+pid. Logic is otherwise identical for
/// both wrappers.
fn adopt_filtered(reg: &Registry, only: Option<&[AdoptKey]>) -> Result<Adoption> {
    let infos = transport::enumerate().context("HID enumeration failed")?;
    let mut out = Adoption::default();
    // The dedupe ledger keys on the FULL adoption identity, not bare pid: two families sharing a
    // pid each get their own probe (marking one tried must not veto the other's sibling pipe).
    let mut tried: Vec<AdoptKey> = Vec::new();
    for i in &infos {
        // Route by CLAIMING, not a hardcoded razer signature: the first dialect whose `claims()`
        // is true owns this pipe and will `probe()` it. A pipe no family claims is not ours to
        // synthesize (it may still be interested-but-unclaimed — that's the ledger's job, not
        // adoption's). This is the generalization: adding a dialect makes its pipes adoptable here.
        let Some(d) = dialects().iter().copied().find(|d| d.claims(i)) else {
            continue;
        };
        // The adoption identity of THIS pipe. `d` is the claiming dialect the loop just resolved —
        // identical to what `adopt_key(i)` would recompute (both use the first claiming dialect),
        // so we reuse it rather than re-run `claimed_by`. A pipe no family claims already skipped
        // above, so it can never match a caller's key — the unchanged "not ours" skip.
        let key: AdoptKey = (d.id(), i.pid);
        // Filtered pass: leave keys this caller wasn't asked to adopt to whoever owns them.
        if only.is_some_and(|keys| !keys.contains(&key)) {
            continue;
        }
        // Already-known is a FAMILY question, not a pid one: this pipe is covered only when ITS
        // claiming family (`d.id()` = key.0) already has a loaded def. A razer def on this pid must
        // NOT suppress adopting the same physical unit's second-family pipe (the review-blocking
        // suppression `find_by_pid(...).is_some()` caused). `knows_family` restores that.
        if reg.knows_family(i.vid, i.pid, d.id()) || tried.contains(&key) {
            continue;
        }
        let file = auto_def_path(d.id(), i.pid);
        if file.exists() {
            // The registry didn't match it, yet its auto file exists — the file is broken or
            // shadow-named. Never clobber user-owned config; report and move on.
            tried.push(key);
            out.skipped.push(format!(
                "pid {:04x}: {} exists but the registry does not resolve it — fix or delete that file",
                i.pid,
                file.display()
            ));
            continue;
        }
        // Several collections share one pid; probe pipes until one talks. Only a talking
        // pipe marks the pid tried — a mute vendor collection must not veto its sibling.
        let Ok(t) = transport::open_path(&i.path) else {
            continue;
        };
        let Some(mut synth) = d.probe(&*t, &SynthCtx::from_info(i)) else {
            continue;
        };
        // Family identity is OWNED by this loop, not trusted from the probe: the file name and
        // the def's `dialect` field both derive from the claiming dialect's `d.id()`, so they
        // can never disagree — in any build profile. (The old debug_assert only *checked* the
        // probe's stamp, which a release build skipped; a future dialect that forgot to stamp
        // would have written a file whose name and field routed the device's bytes through two
        // different exec loops. Assignment makes the mismatch unrepresentable instead.)
        synth.def.dialect = d.id().to_string();
        tried.push(key);
        let write = std::fs::create_dir_all(file.parent().expect("auto path has a parent"))
            .and_then(|()| std::fs::write(&file, emit_toml(&synth)));
        match write {
            Ok(()) => out.adopted.push(Adopted {
                pid: i.pid,
                dialect: d.id(),
                name: synth.def.name.clone(),
                path: file,
            }),
            Err(e) => out
                .skipped
                .push(format!("pid {:04x}: writing {} failed: {e}", i.pid, file.display())),
        }
    }
    Ok(out)
}

/// Are there connected control pipes SOME dialect claims but the registry doesn't recognize?
/// Cheap (one HID enumeration, no device I/O) — the app's scan tick uses this to decide whether
/// adoption is worth spawning at all. Generalized the same way as [`adopt_filtered`]: a pipe is
/// adoptable iff a dialect claims it (was: the hardcoded razer signature), so a new dialect's
/// unknown pipes light this up with no change here.
pub fn unknown_present(reg: &Registry) -> Vec<u16> {
    let Ok(infos) = transport::enumerate() else {
        return Vec::new();
    };
    let mut pids: Vec<u16> = infos
        .iter()
        // A pipe is adoptable iff a family claims it AND that CLAIMING family isn't yet loaded.
        // Family-scoped (not `find_by_pid(...).is_none()`): a second family on an already-known pid
        // must still light up the "adoption worth spawning" signal, or it could never be adopted.
        .filter_map(|i| crate::dialect::claimed_by(i).map(|d| (i, d)))
        .filter(|(i, d)| !reg.knows_family(i.vid, i.pid, d.id()))
        .map(|(i, _)| i.pid)
        .collect();
    pids.sort_unstable();
    pids.dedup();
    pids
}

/// One pipe that some dialect is INTERESTED in (our vendor's hardware) but NONE claims (no
/// protocol we speak) and the registry can't resolve — the raw enumeration facts, no device I/O.
pub struct UnclaimedPipe {
    pub vid: u16,
    pub pid: u16,
    pub product: String,
    pub feature_len: u16,
    pub usage_page: u16,
    pub usage: u16,
}

/// The UNCLAIMED LEDGER: every connected pipe a dialect is `interested` in but none `claims`,
/// and which the registry doesn't resolve. This is the data source for the failed-adoption
/// surface (wave 2b — the dim "our vendor's hardware, no protocol we speak" row) and the parking
/// spot the audio-sidecar recon (DIALECT-RND §audio: the 41-byte sound card, the 64-byte Seiren)
/// reads from. Pure enumeration — dedupe by (pid, usage_page, usage), no probing (you cannot
/// safely sweep a framing you don't know).
pub fn unclaimed_pipes(reg: &Registry) -> Vec<UnclaimedPipe> {
    let Ok(infos) = transport::enumerate() else {
        return Vec::new();
    };
    unclaimed_from(reg, &infos)
}

/// The ledger computed over an ENUMERATION the caller already has — the no-double-enumerate half of
/// [`unclaimed_pipes`] (which delegates here after enumerating). The app's `scan_devices` is already
/// holding its HID enumeration when it wants the failed-adoption rows, so it calls THIS and avoids a
/// second `transport::enumerate()` per scan tick. Pure over the slice; same keep/dedupe rule.
pub fn unclaimed_from(reg: &Registry, infos: &[HidDeviceInfo]) -> Vec<UnclaimedPipe> {
    // The DRIVEN-UNIT shield, unit-identity edition (Ruling C.2). The old `find_by_pid(...).is_some()`
    // suppression kept a RECOGNIZED device's sibling collections (the Naga alone has ~11 non-control
    // pipes, all interested-but-unclaimed) out of the footnote — but that shield is dialect-blind and
    // would wrongly veto a genuine second-family pipe. The physically true rule is UNIT identity: a
    // pipe is a driven unit's own sibling (not inventory) iff its `path_instance` matches a collection
    // that some loaded def RESOLVES as a control pipe. Precompute that instance set ONCE — O(n)
    // find_for_pipe calls over the enumeration — instead of resolving per pipe (which would be O(n²)
    // resolver calls). ACCEPTED EDGE: a second-family pipe on an already-DRIVEN unit stays OFF the
    // ledger; that's fine — it is adoptable via `knows_family` once its dialect exists (the ledger is
    // for hardware with NO path forward, e.g. the Seiren / sound card — their own units, no resolvable
    // control pipe — which correctly stay listed).
    let driven: std::collections::HashSet<String> = infos
        .iter()
        .filter(|c| reg.find_for_pipe(c).is_some())
        .map(|c| c.instance())
        .collect();
    let mut out: Vec<UnclaimedPipe> = Vec::new();
    for i in infos {
        // Kept iff SOME dialect is curious (our vendor) but NONE can frame it, and the pipe isn't a
        // sibling of a driven unit. A pipe no family is even interested in is simply not ours.
        let interested = dialects().iter().any(|d| d.interested(i));
        let claimed = dialects().iter().any(|d| d.claims(i));
        if !interested || claimed || driven.contains(&i.instance()) {
            continue;
        }
        // ONE row per pid, pointing at the pipe worth reconning: the row's byte-size is a recon hint,
        // so keep the LARGEST feature_len (the vendor-protocol candidate). The old keep-first rule
        // labeled the user's sound card by its 0-byte consumer-control pipe — which enumerated ahead
        // of the interesting 41-byte vendor pipe on the same pid.
        if let Some(existing) = out.iter_mut().find(|u| u.pid == i.pid) {
            if i.feature_len > existing.feature_len {
                *existing = UnclaimedPipe {
                    vid: i.vid,
                    pid: i.pid,
                    product: i.product.clone(),
                    feature_len: i.feature_len,
                    usage_page: i.usage_page,
                    usage: i.usage,
                };
            }
            continue; // dedupe sibling collections of the same pipe (keep the fattest)
        }
        out.push(UnclaimedPipe {
            vid: i.vid,
            pid: i.pid,
            product: i.product.clone(),
            feature_len: i.feature_len,
            usage_page: i.usage_page,
            usage: i.usage,
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{Report, BUF_LEN};
    use std::sync::Mutex;

    /// A scripted razer_report device: answers SUCCESS (with canned args) for the (class, id)
    /// pairs it "supports", UNSUPPORTED for everything else — the same yes/no contract the
    /// real firmware gives the probe.
    struct MockDevice {
        supports: Vec<(u8, u8, [u8; 80])>,
        last: Mutex<Option<(u8, u8)>>,
    }

    impl MockDevice {
        fn new(cmds: &[(u8, u8)]) -> Self {
            MockDevice {
                supports: cmds.iter().map(|&(c, i)| (c, i, [0u8; 80])).collect(),
                last: Mutex::new(None),
            }
        }
        fn with_args(mut self, class: u8, id: u8, args: &[u8]) -> Self {
            for (c, i, a) in &mut self.supports {
                if *c == class && *i == id {
                    a[..args.len()].copy_from_slice(args);
                }
            }
            self
        }
    }

    impl Transport for MockDevice {
        fn set_feature(&self, buf: &[u8]) -> anyhow::Result<()> {
            *self.last.lock().unwrap() = Some((buf[7], buf[8]));
            Ok(())
        }
        fn get_feature(&self, buf: &mut [u8]) -> anyhow::Result<()> {
            let Some((class, id)) = *self.last.lock().unwrap() else {
                anyhow::bail!("no pending command");
            };
            let mut rep = Report::command(PROBE_TX, class, id, 0);
            match self.supports.iter().find(|(c, i, _)| *c == class && *i == id) {
                Some((_, _, args)) => {
                    rep.status = 0x02; // SUCCESS
                    rep.args = *args;
                }
                None => rep.status = 0x05, // UNSUPPORTED
            }
            let out = rep.to_buf();
            let n = buf.len().min(BUF_LEN);
            buf[..n].copy_from_slice(&out[..n]);
            Ok(())
        }
    }

    fn ctx(pid: u16, product: &str) -> SynthCtx {
        SynthCtx {
            vid: RAZER_VID,
            pid,
            usage_page: 0x0001,
            usage: 0x0002,
            feature_len: RAZER_FEATURE_LEN,
            product: product.into(),
        }
    }

    /// A matrix-era wireless mouse shaped like the Naga V2 Pro (getter space only).
    fn mock_matrix_mouse() -> MockDevice {
        MockDevice::new(&[
            (0x00, 0x81), (0x00, 0x82), (0x00, 0x84), (0x00, 0x85), (0x00, 0xC0),
            (0x04, 0x85), (0x04, 0x83), (0x04, 0x86),
            (0x07, 0x80), (0x07, 0x84),
            (0x06, 0x8E), (0x06, 0x80), (0x06, 0x8D),
            (0x0F, 0x84), (0x0F, 0x82),
        ])
        .with_args(0x00, 0x82, b"UNIT12345SERIAL")
    }

    /// A legacy-era wired keyboard shaped like the BlackWidow Chroma V2. Answers the FIRMWARE
    /// GAME MODE getter (0x03/0x80) too — the Win-key-kill probe legacy boards respond to.
    fn mock_legacy_keyboard() -> MockDevice {
        MockDevice::new(&[
            (0x00, 0x81), (0x00, 0x82), (0x00, 0x84),
            (0x03, 0x80), (0x03, 0x88), (0x03, 0x89),
        ])
    }

    #[test]
    fn synthesizes_matrix_mouse_matching_the_curated_naga_def() {
        let s = synthesize(&mock_matrix_mouse(), &ctx(0x00A8, "Razer Naga V2 Pro")).unwrap();
        let d = &s.def;
        // The curated Naga def is the ground truth this synthesis must reproduce.
        let naga: DeviceDef =
            toml::from_str(include_str!("../devices/razer-naga-v2-pro.toml")).unwrap();
        for name in [
            "firmware_version", "serial", "device_mode", "polling_rate", "set_polling",
            "polling2", "set_polling2", "dpi", "set_dpi", "dpi_stages", "set_dpi_stages",
            "dpi_stages_active", "battery_level", "charging_status", "storage_info",
            "brightness", "set_brightness", "lighting_state",
        ] {
            let (a, b) = (d.command(name), naga.command(name));
            let a = a.unwrap_or_else(|| panic!("synth missing '{name}'"));
            let b = b.unwrap_or_else(|| panic!("curated missing '{name}'"));
            assert_eq!(
                (a.class, a.id, a.size, &a.args),
                (b.class, b.id, b.size, &b.args),
                "spec mismatch for '{name}'"
            );
        }
        assert_eq!(d.transaction_id, naga.transaction_id, "matrix era tx");
        let (l, nl) = (d.lighting.as_ref().unwrap(), naga.lighting.as_ref().unwrap());
        assert_eq!(l.protocol, Protocol::Matrix);
        assert_eq!((l.led_id, l.custom_id, l.varstore), (nl.led_id, nl.custom_id, nl.varstore));
        assert_eq!(l.effects, nl.effects);
        assert_eq!(l.effect, nl.effect);
        assert_eq!(l.custom_frame, nl.custom_frame);
        assert_eq!(l.brightness, nl.brightness);
        assert_eq!((l.rows, l.cols), (nl.rows, nl.cols), "dpi-present heuristic = mouse dims");
        assert_eq!(d.name, "Razer Naga V2 Pro");
        assert_eq!(s.serial, "UNIT12345SERIAL");
        // Battery-bearing = wireless-capable -> gets the receiver pacing even though the mock
        // answers instantly (the live Naga dongle ACKs getters in ~6ms yet still flickers
        // without it). Must equal the curated def's hardware-calibrated value.
        assert_eq!(d.stream_wait_us, naga.stream_wait_us);
        // Capture-only knowledge is NOT synthesized — it stays curated config.
        assert!(d.command("set_scroll_stage").is_none());
        assert!(d.side_plates.is_none());
        // No firmware game mode on a matrix mouse: it never answers the legacy class-0x03 getter,
        // so the probe grows neither the getter nor its setter (no fake Win-key kill on a mouse).
        assert!(d.command("game_mode").is_none());
        assert!(d.command("set_game_mode").is_none());
    }

    #[test]
    fn synthesizes_legacy_keyboard_matching_the_curated_blackwidow_def() {
        use crate::registry::Capability;
        let s = synthesize(&mock_legacy_keyboard(), &ctx(0x0221, "")).unwrap();
        let d = &s.def;
        let bw: DeviceDef =
            toml::from_str(include_str!("../devices/razer-blackwidow-chroma-v2.toml")).unwrap();
        assert_eq!(d.transaction_id, bw.transaction_id, "legacy default tx 0xFF");
        let (l, bl) = (d.lighting.as_ref().unwrap(), bw.lighting.as_ref().unwrap());
        assert_eq!(l.protocol, Protocol::Legacy);
        assert_eq!(l.effects, bl.effects);
        assert_eq!(l.effect, bl.effect, "split-brain 0x3F lighting tx");
        assert_eq!(l.custom_frame, bl.custom_frame);
        // The legacy dialect has NO brightness getter, so the probe can't prove a brightness
        // write — the synthesized block omits it even though the CURATED def keeps Some (that
        // 0x03/0x03 spec was verified on the physical board, per-device evidence a blind probe
        // can't reproduce). Since `lighting.brightness` now grants SetBrightness, emitting it
        // untested would put unproven bytes on the wire; without it the def honestly declines.
        assert!(
            l.brightness.is_none(),
            "legacy synthesis has no getter to prove a brightness write against"
        );
        assert!(bl.brightness.is_some(), "curated BW keeps its board-verified brightness write");
        assert!(
            !d.supports(Capability::SetBrightness),
            "no proven brightness getter -> no brightness write capability"
        );
        // legacy lighting_state resolves to the class-0x03 getter (no matrix on this board)
        let st = d.command("lighting_state").unwrap();
        assert_eq!((st.class, st.id), (0x03, 0x88));
        // FIRMWARE GAME MODE — the board answered the 0x03/0x80 getter, so the synthesized def gains
        // the getter (with its baked [varstore, GAME_LED] args) AND its paired setter, and supports
        // BOTH new capabilities (the Win-key-kill surface a legacy keyboard exposes, a mouse can't).
        let gm = d.command("game_mode").unwrap();
        assert_eq!(
            (gm.class, gm.id, gm.size, &gm.args[..]),
            (0x03, 0x80, 0x03, &[0x00u8, 0x08][..])
        );
        let sgm = d.command("set_game_mode").unwrap();
        assert_eq!((sgm.class, sgm.id, sgm.size), (0x03, 0x00, 0x03));
        assert!(d.supports(Capability::GameMode));
        assert!(d.supports(Capability::SetGameMode));
        // no product string -> honest placeholder name
        assert_eq!(d.name, "Razer device 0221 (auto)");
        // nothing mouse-shaped leaked in
        assert!(d.command("dpi").is_none());
        assert!(d.command("battery_level").is_none());
    }

    #[test]
    fn matrix_without_brightness_getter_gets_no_brightness_write() {
        // The exact review scenario: a matrix board whose lighting_state getter (0x0F/0x82)
        // answers — so the dialect is Matrix and there IS a lighting block — but whose brightness
        // getter (0x0F/0x84) does NOT. The brightness getter gates the top-level `set_brightness`
        // command AND (now) the block's brightness write, so an unproven getter must leave BOTH
        // absent, and `supports(SetBrightness)` must read false. Emitting the block's brightness
        // on era-inference alone (the old behaviour) put unproven bytes behind that capability.
        use crate::registry::Capability;
        let mock = MockDevice::new(&[(0x00, 0x81), (0x0F, 0x82)]);
        let s = synthesize(&mock, &ctx(0x1234, "Razer matrix (no brightness getter)")).unwrap();
        let d = &s.def;
        let l = d.lighting.as_ref().expect("lighting_state answered -> a matrix block");
        assert_eq!(l.protocol, Protocol::Matrix);
        assert!(d.command("set_brightness").is_none(), "unproven getter -> no setter");
        assert!(l.brightness.is_none(), "unproven getter -> no block brightness write");
        assert!(
            !d.supports(Capability::SetBrightness),
            "no brightness write path anywhere -> supports(SetBrightness) is false"
        );
    }

    #[test]
    fn mute_pipe_synthesizes_nothing() {
        let mock = MockDevice::new(&[]); // answers UNSUPPORTED to everything
        assert!(synthesize(&mock, &ctx(0x9999, "x")).is_none());
    }

    #[test]
    fn emitted_toml_round_trips_to_the_same_def() {
        for (mock, pid, product) in [
            (mock_matrix_mouse(), 0x00A8u16, "Razer Naga V2 Pro"),
            (mock_legacy_keyboard(), 0x0221, ""),
        ] {
            let s = synthesize(&mock, &ctx(pid, product)).unwrap();
            let text = emit_toml(&s);
            let mut parsed: DeviceDef = toml::from_str(&text)
                .unwrap_or_else(|e| panic!("emitted TOML must parse: {e}\n---\n{text}"));
            // `origin` is a LOAD fact, never file content (serde skip) — the synth stamps Auto,
            // a bare parse defaults Builtin. Reconcile it before the equality: this test pins
            // FILE-content losslessness, and origin is precisely the field the file doesn't carry.
            parsed.origin = s.def.origin.clone();
            assert_eq!(parsed, s.def, "emit → parse must be lossless");
        }
    }

    #[test]
    fn toml_escape_neutralizes_toml_metacharacters() {
        // quote and backslash get their TOML escapes (they round-trip losslessly)…
        assert_eq!(toml_escape("a\"b"), "a\\\"b");
        assert_eq!(toml_escape("a\\b"), "a\\\\b");
        // …control bytes become a space (a name carrying them is garbage — keep the file readable).
        assert_eq!(toml_escape("a\nb"), "a b");
        assert_eq!(toml_escape("a\tb"), "a b");
        assert_eq!(toml_escape("a\u{7F}b"), "a b");
        // the escaped body must actually parse as a TOML basic string.
        let v: toml::Value = format!("k = \"{}\"", toml_escape("x\"y\\z\nw")).parse().unwrap();
        assert_eq!(v["k"].as_str().unwrap(), "x\"y\\z w");
    }

    #[test]
    fn emitted_toml_survives_a_hostile_product_string() {
        // A device is free to report a name carrying quotes, a backslash, and raw control bytes.
        // Interpolated raw, that name emits INVALID TOML — Registry::load then silently skips the
        // file while adopt_unknown refuses to clobber it, leaving the device permanently
        // un-adopted. The emitted file must PARSE, and the parsed def must equal the synthesized
        // one: the name's control byte is space-replaced in BOTH (the file is the source of truth,
        // and synthesize already mirrors what a reload of the file yields).
        let s = synthesize(
            &mock_matrix_mouse(),
            &ctx(0x00A8, "Weird \"Product\" \\ v2\nrev"),
        )
        .unwrap();
        assert_eq!(s.def.name, "Weird \"Product\" \\ v2 rev", "control byte → space, quotes/\\ kept");
        let text = emit_toml(&s);
        let mut parsed: DeviceDef = toml::from_str(&text)
            .unwrap_or_else(|e| panic!("hostile product must still emit valid TOML: {e}\n---\n{text}"));
        // origin is serde-skipped (a load fact) — reconcile it; the file-content is what's lossless.
        parsed.origin = s.def.origin.clone();
        assert_eq!(parsed, s.def, "emit → parse must be lossless even for a hostile name");
        // The `name = "…"` line must be whole — a raw newline in the value would split it, so a
        // found line that stops before its closing quote is the bug this fix prevents.
        let name_line = text
            .lines()
            .find(|l| l.starts_with("name = "))
            .expect("emitted file has a name line");
        assert!(name_line.ends_with('"'), "name value split by a raw newline: {name_line:?}");
    }

    #[test]
    fn stream_wait_tracks_link_and_wireless_capability() {
        assert_eq!(stream_wait_for(1, false), 0, "wired-class round-trip, no battery");
        assert_eq!(stream_wait_for(8, false), 0);
        assert_eq!(stream_wait_for(15, false), WIRELESS_STREAM_WAIT_US, "slow link alone");
        assert_eq!(stream_wait_for(32, false), WIRELESS_STREAM_WAIT_US);
        // battery-bearing board on a FAST link still gets the pacing (the live Naga case)
        assert_eq!(stream_wait_for(1, true), WIRELESS_STREAM_WAIT_US);
    }

    #[test]
    fn synthesized_commands_satisfy_the_capability_map() {
        // Every capability the mock mouse's getter space implies must resolve through the
        // SAME registry-driven gate the GUI/CLI use — synthesis feeds that gate, so the
        // names must line up exactly (a typo here would silently grey-out real features).
        use crate::registry::Capability;
        let s = synthesize(&mock_matrix_mouse(), &ctx(0x00A8, "n")).unwrap();
        for cap in [
            Capability::Dpi, Capability::SetDpi, Capability::DpiStages,
            Capability::SetDpiStages, Capability::Polling, Capability::SetPolling,
            Capability::Polling2, Capability::SetPolling2, Capability::Brightness,
            Capability::SetBrightness, Capability::Battery, Capability::Storage,
            Capability::Lighting,
        ] {
            assert!(s.def.supports(cap), "synthesized def must support {cap:?}");
        }
        // and the one capability synthesis can't discover stays absent
        assert!(!s.def.supports(Capability::SetScrollStage));
    }

    #[test]
    fn proven_commands_are_exactly_the_answered_catalog_result() {
        // `proven_commands` is a `Proven<Vec<String>>` minted INSIDE `synthesize` — the only place
        // holding a probe answer. `Proven::mint` is PRIVATE to the synth module, so no downstream
        // code can forge one (there is no public constructor; try to write `Proven::mint(vec![])`
        // from a test in another crate and it won't compile). That privacy is the compile-time
        // honesty property; here we assert the mint PATH produces the right set.
        let s = synthesize(&mock_matrix_mouse(), &ctx(0x00A8, "n")).unwrap();
        let proven = s.proven_commands.get();
        // (a) it carries the getters the mock answered SUCCESS for, plus the setters they unlock…
        for name in ["dpi", "set_dpi", "battery_level", "brightness", "set_brightness", "lighting_state"] {
            assert!(proven.iter().any(|c| c == name), "proven_commands missing answered '{name}'");
        }
        // …and NOT the catalog names the mock never answered (legacy-only class-0x03 lighting).
        for name in ["lighting_caps"] {
            assert!(!proven.iter().any(|c| c == name), "proven_commands has unanswered '{name}'");
        }
        // (b) the mint path: proven_commands is EXACTLY the def's command map keys (the def is the
        // data, this is the evidence of how it was built — same set, two representations).
        use std::collections::BTreeSet;
        let keys: BTreeSet<&str> = s.def.commands.keys().map(String::as_str).collect();
        let proven_set: BTreeSet<&str> = proven.iter().map(String::as_str).collect();
        assert_eq!(proven_set, keys, "proven_commands must equal def.commands keys");
    }
}
