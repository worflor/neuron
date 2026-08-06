// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

//! Protocol FAMILIES as a pluggable seam. Everything above the wire stays semantic
//! (`Capability::*`, the registry, FEEL/LIGHTING, profiles, adoption UX); everything
//! wire-shaped — bus signature, framing, status semantics, exec discipline — lives behind a
//! [`Dialect`]. `razer_report` is dialect #1, its production exec loops MOVED here verbatim from
//! `device.rs` so every existing device's traffic is byte-identical (pinned by the frame goldens
//! below). Semantics live above (Capability — a HID++ mouse's 0x1000 battery and a Razer mouse's
//! 0x07/0x80 battery are the SAME `Capability::Battery` to everything above this line); bytes live
//! here. See docs/TDD.md §5.7 for the dialect design and the wave plan.

use crate::protocol::{reply_status, Report, Status, BUF_LEN};
use crate::transport::{HidDeviceInfo, Transport};
use anyhow::{bail, Result};
use std::time::Duration;

/// Razer's USB vendor id — the razer_report bus signature's first half. Canonical home is here
/// (the dialect that owns the signature); `synth.rs` keeps its own copy until the wave-2
/// consolidation folds direct consumers onto the dialect.
pub const RAZER_VID: u16 = 0x1532;
/// The universal razer_report control-pipe signature: a 91-byte feature report. (Same note as
/// [`RAZER_VID`] — duplicated in `synth.rs` until wave 2.)
pub const RAZER_FEATURE_LEN: u16 = 91;

/// A wire-protocol FAMILY: how to recognize its pipes, frame its commands, and read its replies.
/// One impl per family; devices reference their family by id in `DeviceDef.dialect` (registry
/// DATA, serde-default "razer"). Object-safe so a `&'static [&'static dyn Dialect]` registry can
/// hold every family and callers resolve one by id.
///
/// WAVE 2 landed the probing seam: `probe` is now on the trait (the per-family generalization of
/// `synth::synthesize`) and the adoption pipeline routes every unknown pipe to its claiming
/// dialect. `interested` widens recognition to VENDOR granularity for the unclaimed ledger.
pub trait Dialect: Send + Sync {
    /// Stable family id, matched against `DeviceDef.dialect` (registry data). "razer" | "hidpp" | …
    fn id(&self) -> &'static str;

    /// Bus signature: does this HID pipe look like mine? (razer: VID 0x1532 + a 91-byte feature
    /// report.) Claiming is how the generalized adoption pipeline (wave 2) routes an unknown pipe
    /// to the family that can probe it.
    fn claims(&self, info: &HidDeviceInfo) -> bool;

    /// VENDOR-level curiosity, one rung wider than pipe-level [`claims`]. Default: exactly
    /// `claims` — a family that can frame a pipe obviously recognizes it. A dialect OVERRIDES this
    /// to widen to its whole vendor when it recognizes HARDWARE whose pipe SHAPES it cannot (yet)
    /// speak — Razer's audio sidecars enumerate under vid 0x1532 but with 41/64-byte reports, not
    /// the 91-byte razer_report control pipe. Interested-but-unclaimed pipes feed the UNCLAIMED
    /// LEDGER ([`crate::synth::unclaimed_pipes`]): the honest "this IS our vendor's hardware, but
    /// no protocol we speak" surface (the wave-2b failed-adoption row), as opposed to a pipe no
    /// family is even curious about, which is simply not ours.
    fn interested(&self, info: &HidDeviceInfo) -> bool {
        self.claims(info)
    }

    /// Is `info` the CONTROL pipe that `def` drives? Which HID collection of a composite device is a
    /// def's control pipe is a PER-FAMILY question, not one triple compared universally: razer picks
    /// its pipe by the `usage_page`/`usage`/91-byte-feature-report triple stored in the def; hidpp
    /// picks its by the report-SHAPE claim (VID + the 7/20-byte output/input reports) — its def's
    /// stored `feature_report_len` is MEANINGLESS there (HID++ rides output/input reports, never
    /// feature reports; the length is kept only for completeness, per `hidpp.rs`). `DeviceDef::
    /// matches_control` routes here so no call site has to know which family's rule applies; a def
    /// whose dialect id resolves to no family matches nothing (fail closed — see `by_id` /
    /// `DeviceDef::matches_control`, and Finding 2's fail-closed framing).
    fn matches_control(&self, def: &crate::registry::DeviceDef, info: &HidDeviceInfo) -> bool;

    /// Probe an unclaimed pipe's getter space and synthesize its complete `DeviceDef`. Read-only
    /// (getters only — never sweep an unknown framing with writes). `None` = the pipe never
    /// answered, so it is not a talking pipe of this family. The per-family generalization of
    /// [`crate::synth::synthesize`]; the claiming adoption pass calls this on the dialect that
    /// `claims` the pipe. (In-crate module cycle synth↔dialect is fine — same crate.)
    fn probe(
        &self,
        t: &dyn Transport,
        ctx: &crate::synth::SynthCtx,
    ) -> Option<crate::synth::Synthesis>;

    /// One framed, acknowledged command round-trip → the 80-byte reply body. The byte fields
    /// (class/id/size/args and the transaction id) are dialect-interpreted. WAVE-1 SIGNATURE NOTE:
    /// this keeps the raw-parts shape `Device` already speaks (not `&CommandSpec`) — the `Device`
    /// methods pass exactly what they pass today, so routing through the dialect cannot change a
    /// single byte. Wave 2 may lift this to spec-level once the whole exec surface is behind the
    /// seam.
    fn exec(
        &self,
        t: &dyn Transport,
        transaction_id: u8,
        spec_class: u8,
        spec_id: u8,
        size: u8,
        args: &[u8],
    ) -> Result<[u8; 80]>;

    /// The fire-and-drain streaming write (lighting frames): send, wait the link's round-trip,
    /// drain the reply ONCE (no retry loop). The wait discipline (`stream_wait_us`) is registry
    /// data passed in by the caller — not read from a def here, so this stays a pure wire op.
    fn exec_fast(
        &self,
        t: &dyn Transport,
        transaction_id: u8,
        class: u8,
        id: u8,
        size: u8,
        args: &[u8],
        stream_wait_us: u64,
    );

    /// Release the family's CUSTODY of a device back to firmware — the rest-state restore that
    /// runs at stream teardown and app exit (DIALECT-RND "Device-mode lifecycle"). Per-family
    /// because custody is a per-family concept: razer's driver mode is a LEASE this hook returns
    /// (device_mode -> 0x00, re-enabling onboard buttons/FN and firmware wake-restore); HID++
    /// devices are never in our custody (no mode concept — the default no-op IS the correct
    /// release). A dialect that takes custody in exec/streaming MUST override this.
    fn release_custody(&self, t: &dyn Transport, def: &crate::registry::DeviceDef) -> Result<()> {
        let _ = (t, def);
        Ok(())
    }

    /// The dialect's own device-PUSHED event vocabulary, when it has one that applies FAMILY-WIDE —
    /// as opposed to a per-device registry `[events]` block (`DeviceDef::event_for`), which is a
    /// per-device OVERRIDE checked first by callers (`hidwatch::decode`). Default `None`: razer and
    /// hidpp push nothing this seam knows about (their existing per-device `[events]` path is
    /// unaffected). A dialect that DOES know a family-wide push vocabulary — e.g. razer-audio's tap-
    /// mute report, HARDWARE-CAPTURED on the Seiren V3 Mini (2026-07-08) and heuristically assumed
    /// family-wide until a counterexample — overrides this alongside [`Dialect::pushes_events`].
    fn default_event_for(&self, report: &[u8]) -> Option<crate::registry::EventKind> {
        let _ = report;
        None
    }

    /// Does this dialect push HID input reports at all on `info`'s collection? The FAMILY-level arming
    /// question [`event_dialect_for`] answers, alongside a def's own `event_pipe_matches` (the per-
    /// device question) — `hidwatch::arm_new` arms a collection when EITHER says yes. Default `false`:
    /// a dialect with no family-wide push vocabulary never arms a collection on its own say-so.
    fn pushes_events(&self, info: &HidDeviceInfo) -> bool {
        let _ = info;
        false
    }

    /// The audio-mute WRITE facet: is there a PROVEN setter for this family's tap-mute? Default
    /// `false` — a mute setter is never assumed, and NEVER destructively probed at runtime; a family
    /// earns `true` only by overriding this with a hardware-verified command. Paired with
    /// [`read_audio_mute`] (the READ facet); the UI (glue's `endpoint_mute_writable`) renders an
    /// interactive control only when both facets are real.
    fn audio_mute_writable(&self) -> bool {
        false
    }

    /// The audio-mute authoritative READ: ask the device its CURRENT mute state right now, rather than
    /// waiting for the next push. Default `None` — a dialect with no such getter (or no audio concept
    /// at all) has nothing to answer with. `Err`/timeout from an override should also collapse to
    /// `None` (device asleep / not this family) — this is a best-effort seed, never a hard requirement.
    fn read_audio_mute(&self, t: &dyn Transport) -> Option<bool> {
        let _ = t;
        None
    }
}

/// Build the outgoing 91-byte razer_report feature buffer once, in ONE place. Both [`exec`] and
/// [`exec_fast`] frame through here (DIALECT-RND ruling: `send_lighting_fast`'s duplicated
/// `Report::command → to_buf` build collapses to this). Args past the 80-byte body are dropped,
/// exactly as `Report::command` + the old per-loop copy did.
fn frame(tx: u8, class: u8, id: u8, size: u8, args: &[u8]) -> [u8; BUF_LEN] {
    let mut req = Report::command(tx, class, id, size);
    for (i, b) in args.iter().enumerate() {
        if i < req.args.len() {
            req.args[i] = *b;
        }
    }
    req.to_buf()
}

/// The razer device-mode opcode pair + the NORMAL (firmware-owned rest) mode byte. This is razer
/// WIRE knowledge, so its canonical home is the dialect (bytes live here). `writes::set_device_mode`
/// is the Device-level CONVENIENCE built on this exact framing for the razer-explicit tool paths
/// (`ensure_driver`, the CLI `mode` verb); it is deliberately NOT imported here so the dependency
/// arrow stays writes → dialect and never loops back. (0x00/0x04 = [mode, 0x00]; 0x00 = normal,
/// 0x03 = driver — same pair `ensure_driver` flips, verified live.)
const DEVICE_MODE_CLASS: u8 = 0x00;
const DEVICE_MODE_SET_ID: u8 = 0x04;
const DEVICE_MODE_NORMAL: u8 = 0x00;

/// The razer_report family: the 90-byte vendor report, its CRC, and the busy-poll exec discipline.
/// The only family with hardware on this desk today; every builtin/auto def speaks it.
pub struct RazerDialect;

impl Dialect for RazerDialect {
    fn id(&self) -> &'static str {
        "razer"
    }

    fn claims(&self, info: &HidDeviceInfo) -> bool {
        info.vid == RAZER_VID && info.feature_len == RAZER_FEATURE_LEN
    }

    /// The razer_report control-pipe rule: the def's stored `usage_page`/`usage`/`feature_report_len`
    /// triple must equal the enumerated collection's. (This is the exact comparison that used to live
    /// inline in `DeviceDef::matches_control`, moved behind the seam so HID++ can answer differently.)
    fn matches_control(&self, def: &crate::registry::DeviceDef, info: &HidDeviceInfo) -> bool {
        let c = &def.control_interface;
        c.usage_page == info.usage_page
            && c.usage == info.usage
            && c.feature_report_len == info.feature_len
    }

    /// Widen to the whole Razer VENDOR: any 0x1532 pipe is Razer hardware even when its report
    /// shape is not razer_report (the 41-byte USB sound card, the 64-byte Seiren). Those pipes are
    /// interested-but-unclaimed until their own dialects exist — they belong on the unclaimed
    /// ledger, not invisible.
    fn interested(&self, info: &HidDeviceInfo) -> bool {
        info.vid == RAZER_VID
    }

    /// Delegate to the existing, hardware-proven razer probe. `synthesize` already stamps
    /// `dialect: "razer"` on the def it emits, so the claiming pass's mistag debug_assert holds.
    fn probe(
        &self,
        t: &dyn Transport,
        ctx: &crate::synth::SynthCtx,
    ) -> Option<crate::synth::Synthesis> {
        crate::synth::synthesize(t, ctx)
    }

    fn exec(
        &self,
        t: &dyn Transport,
        transaction_id: u8,
        spec_class: u8,
        spec_id: u8,
        size: u8,
        args: &[u8],
    ) -> Result<[u8; 80]> {
        // Hold the pipe's wire lock for the WHOLE conversation (set → poll/drain [→ re-arm]):
        // pair-atomicity is the unit; between conversations other actors may interleave freely.
        let wire = t.wire_lock();
        let _wire = wire.as_ref().map(|w| w.acquire());

        // MOVED VERBATIM from device.rs::exec_dynamic_tx: 10ms × 60 polls, re-arm at i%12==11,
        // the exact error strings. Only the hand-rolled echo `if b[7]==.. && b[8]==..` collapses
        // into `protocol::reply_status` (same bytes, same decision).
        let cmd_class = spec_class;
        let cmd_id = spec_id;
        let out = frame(transaction_id, spec_class, spec_id, size, args);
        t.set_feature(&out)?;
        for i in 0..60 {
            std::thread::sleep(Duration::from_millis(10));
            let mut b = [0u8; BUF_LEN];
            b[0] = 0x00; // report id for the GET
            if t.get_feature(&mut b).is_ok() {
                // accept only a reply that echoes our class/id (filters cross-talk)
                if let Some(status) = reply_status(&b, cmd_class, cmd_id) {
                    match status {
                        Status::Success => return Ok(Report::from_buf(&b).args),
                        Status::Fail => {
                            bail!("device reported FAIL for command {cmd_class:#04x}/{cmd_id:#04x}")
                        }
                        Status::Unsupported => {
                            bail!("command {cmd_class:#04x}/{cmd_id:#04x} unsupported")
                        }
                        _ => {} // busy / timeout / new — keep polling
                    }
                }
            }
            if i % 12 == 11 {
                // re-arm if the device stayed busy (wireless round-trip can be slow)
                t.set_feature(&out)?;
            }
        }
        bail!("timed out waiting for reply to {cmd_class:#04x}/{cmd_id:#04x}")
    }

    fn exec_fast(
        &self,
        t: &dyn Transport,
        transaction_id: u8,
        class: u8,
        id: u8,
        size: u8,
        args: &[u8],
        stream_wait_us: u64,
    ) {
        // Hold the pipe's wire lock for the WHOLE conversation (set → poll/drain [→ re-arm]):
        // pair-atomicity is the unit; between conversations other actors may interleave freely.
        let wire = t.wire_lock();
        let _wire = wire.as_ref().map(|w| w.acquire());

        // MOVED from device.rs::send_lighting_fast: SetFeature, wait the receiver round-trip if
        // the def calibrated one, then a single GetFeature drain (the razer_report protocol
        // requires the reply be read before the next write — skip it and the device freezes).
        let out = frame(transaction_id, class, id, size, args);
        if t.set_feature(&out).is_ok() {
            if stream_wait_us > 0 {
                std::thread::sleep(Duration::from_micros(stream_wait_us));
            }
            let mut b = [0u8; BUF_LEN];
            let _ = t.get_feature(&mut b); // drain the reply; don't busy-retry
        }
    }

    /// Return the driver-mode LEASE: device_mode -> NORMAL (0x00), re-enabling the board's onboard
    /// buttons/FN combos and the firmware's own wake-restore (DIALECT-RND "Device-mode lifecycle").
    /// Framed through this dialect's own `exec` (tx = the def's transaction id), so the bytes are
    /// razer_report and the ACK is awaited — byte-identical to the raw `writes::set_device_mode(d,
    /// 0x00)` this REPLACES at teardown, now dialect-routed so a non-razer def can never receive it.
    fn release_custody(&self, t: &dyn Transport, def: &crate::registry::DeviceDef) -> Result<()> {
        self.exec(
            t,
            def.transaction_id,
            DEVICE_MODE_CLASS,
            DEVICE_MODE_SET_ID,
            0x02,
            &[DEVICE_MODE_NORMAL, 0x00],
        )?;
        Ok(())
    }
}

/// The one razer instance the registry hands out. A ZST, so `&RAZER` is a cheap `'static` handle.
static RAZER: RazerDialect = RazerDialect;

/// The Consumer-Control usage pair the razer-audio family's control pipe rides (Seiren V3 Mini,
/// hardware-verified 2026-07-08) — distinct from razer_report's mouse/keyboard vendor placement.
const AUDIO_USAGE_PAGE: u16 = 0x000C;
const AUDIO_USAGE: u16 = 0x0001;
/// The razer-audio family's feature-report length — a 64-byte envelope, not razer_report's 91.
const AUDIO_FEATURE_LEN: u16 = 64;
/// The razer-audio envelope's HID report id (buf[0]) — razer_report's is 0x00.
const AUDIO_REPORT_ID: u8 = 0x07;
/// The razer-audio envelope's total buffer length.
const AUDIO_BUF_LEN: usize = 64;
/// The command BODY inside the audio envelope: buf[9..62], 53 bytes (vs razer_report's 80-byte
/// body at buf[9..89]) — buf[62] is CRC, buf[63] is reserved.
const AUDIO_BODY_LEN: usize = 53;
/// The transaction id [`RazerAudioDialect::read_audio_mute`] frames its getter with — the same
/// era-heuristic 0x1F [`RazerAudioDialect::probe`] stamps on every synthesized def (the one value this
/// family's live session actually saw ACK a getter).
const AUDIO_MUTE_READ_TX: u8 = 0x1F;

/// Razer's SECOND wire family (dialect #3): its audio-peripheral control pipe — a 64-byte feature
/// envelope, HID report id 0x07, on the device's Consumer-Control collection rather than
/// razer_report's mouse/keyboard vendor placement. Hardware-verified live on a Seiren V3 Mini
/// (vid 0x1532 pid 0x056A, 2026-07-08): getters class 0x00 id 0x82 (serial) and 0x84 (device_mode)
/// answered Status Success with class/id echoed. CRITICAL finding from that same session: the
/// device PARROTS Success+echo with all-zero args for ~485 of 512 unknown (class,id) headers — a
/// probe-synthesized command map from this family is untrustworthy and [`RazerAudioDialect::probe`]
/// deliberately synthesizes NONE (see its doc). The family's real user-facing surface is the OS
/// Core-Audio capture endpoint, not a knobbed device row — see [`crate::registry::DeviceDef::
/// is_operable`].
pub struct RazerAudioDialect;

impl Dialect for RazerAudioDialect {
    fn id(&self) -> &'static str {
        "razer-audio"
    }

    /// Bus signature: Razer VID + the Consumer-Control usage pair + the 64-byte feature report.
    /// PIPE-SHAPE only, NEVER a pid check — a new audio sidecar with this same collection shape
    /// adopts automatically, which is the entire point of claiming by signature.
    fn claims(&self, info: &HidDeviceInfo) -> bool {
        info.vid == RAZER_VID
            && info.usage_page == AUDIO_USAGE_PAGE
            && info.usage == AUDIO_USAGE
            && info.feature_len == AUDIO_FEATURE_LEN
    }

    /// The same triple-compare rule razer_report uses (mirrors [`RazerDialect::matches_control`]):
    /// this family also picks its control pipe by the def's stored `usage_page`/`usage`/
    /// `feature_report_len`, compared against the enumerated collection's.
    fn matches_control(&self, def: &crate::registry::DeviceDef, info: &HidDeviceInfo) -> bool {
        let c = &def.control_interface;
        c.usage_page == info.usage_page
            && c.usage == info.usage
            && c.feature_report_len == info.feature_len
    }

    /// Synthesize a MINIMAL, honest def for a claimed audio pipe: no probing at all — the CRITICAL
    /// hardware finding (device module doc) is that this family parrots Success+echo with all-zero
    /// args for the vast majority of (class,id) headers, so a probe loop here would mint a command
    /// map out of noise, not evidence. The returned def carries an EMPTY `[commands]` table and no
    /// lighting, which makes `DeviceDef::is_operable()` false — by design: this device's user-facing
    /// row is its Core-Audio capture endpoint, not a knob-less HID device row (the same rule the
    /// deleted `razer-seiren-v3-mini.toml` builtin stated by hand). `transaction_id` 0x1F is the one
    /// value this session actually saw ACK a getter (era-heuristic, like every synthesized tx).
    fn probe(
        &self,
        t: &dyn Transport,
        ctx: &crate::synth::SynthCtx,
    ) -> Option<crate::synth::Synthesis> {
        let _ = t; // read-only by contract, and there is nothing safe left to read (see doc above)
        use crate::registry::{ControlInterface, DefOrigin, DeviceDef, Mode};
        use crate::synth::{Heuristic, Synthesis};
        use std::collections::BTreeMap;

        let name = if ctx.product.trim().is_empty() {
            format!("Razer audio device {:04x}", ctx.pid)
        } else {
            ctx.product.trim().to_string()
        };
        let def = DeviceDef {
            name,
            codename: format!("auto-{:04x}", ctx.pid),
            dialect: "razer-audio".into(),
            origin: DefOrigin::Auto,
            vendor_id: ctx.vid,
            transaction_id: 0x1F,
            stream_wait_us: 0,
            modes: vec![Mode {
                name: "default".into(),
                product_id: ctx.pid,
            }],
            control_interface: ControlInterface {
                usage_page: ctx.usage_page,
                usage: ctx.usage,
                feature_report_len: ctx.feature_len,
            },
            commands: BTreeMap::new(),
            lighting: None,
            side_plates: None,
            // No push-report vocabulary probed here — the FAMILY vocabulary is on this dialect
            // itself (`default_event_for`), so an empty def still arms correctly via
            // `event_dialect_for`; a per-device `[events]` override is a config addition later.
            events: None,
        };
        Some(Synthesis::from_probe(
            def,
            Vec::new(),
            Heuristic(0x1F),
            None,
            Heuristic(0),
            String::new(),
            0,
        ))
    }

    fn exec(
        &self,
        t: &dyn Transport,
        transaction_id: u8,
        spec_class: u8,
        spec_id: u8,
        size: u8,
        args: &[u8],
    ) -> Result<[u8; 80]> {
        // Hold the pipe's wire lock for the WHOLE conversation (set → poll/drain [→ re-arm]):
        // pair-atomicity is the unit; between conversations other actors may interleave freely.
        let wire = t.wire_lock();
        let _wire = wire.as_ref().map(|w| w.acquire());

        // Mirrors RazerDialect::exec's busy-poll discipline verbatim (10ms x 60 polls, re-arm at
        // i%12==11, the exact error strings) — only the envelope shape differs (64 bytes / report id
        // 0x07 vs razer_report's 91 bytes / report id 0x00). `reply_status` reads offsets 1/7/8,
        // IDENTICAL in both envelopes, so the echo filter is reused unchanged.
        let cmd_class = spec_class;
        let cmd_id = spec_id;
        let out = frame_audio(transaction_id, spec_class, spec_id, size, args);
        t.set_feature(&out)?;
        for i in 0..60 {
            std::thread::sleep(Duration::from_millis(10));
            let mut b = [0u8; AUDIO_BUF_LEN];
            b[0] = 0x00; // report id for the GET, same convention RazerDialect::exec uses
            if t.get_feature(&mut b).is_ok() {
                if let Some(status) = reply_status(&b, cmd_class, cmd_id) {
                    match status {
                        Status::Success => return Ok(args_from_audio_buf(&b)),
                        Status::Fail => {
                            bail!("device reported FAIL for command {cmd_class:#04x}/{cmd_id:#04x}")
                        }
                        Status::Unsupported => {
                            bail!("command {cmd_class:#04x}/{cmd_id:#04x} unsupported")
                        }
                        _ => {} // busy / timeout / new — keep polling
                    }
                }
            }
            if i % 12 == 11 {
                t.set_feature(&out)?;
            }
        }
        bail!("timed out waiting for reply to {cmd_class:#04x}/{cmd_id:#04x}")
    }

    fn exec_fast(
        &self,
        t: &dyn Transport,
        transaction_id: u8,
        class: u8,
        id: u8,
        size: u8,
        args: &[u8],
        stream_wait_us: u64,
    ) {
        // Hold the pipe's wire lock for the WHOLE conversation (set → poll/drain [→ re-arm]):
        // pair-atomicity is the unit; between conversations other actors may interleave freely.
        let wire = t.wire_lock();
        let _wire = wire.as_ref().map(|w| w.acquire());

        // Mirrors RazerDialect::exec_fast: SetFeature, wait the calibrated round-trip, one drain —
        // never a busy-retry loop.
        let out = frame_audio(transaction_id, class, id, size, args);
        if t.set_feature(&out).is_ok() {
            if stream_wait_us > 0 {
                std::thread::sleep(Duration::from_micros(stream_wait_us));
            }
            let mut b = [0u8; AUDIO_BUF_LEN];
            let _ = t.get_feature(&mut b); // drain the reply; don't busy-retry
        }
    }

    // NB: `release_custody` is INTENTIONALLY NOT overridden — the trait's default no-op is correct.
    // This family never takes a driver-mode LEASE (no device-mode concept on an audio peripheral's
    // control pipe), so there is nothing to hand back at teardown — the same reasoning HID++'s
    // `release_custody` doc gives for why IT stays default too.

    /// Any pipe this dialect CLAIMS is also one it PUSHES the family-wide tap-mute vocabulary on —
    /// the two questions coincide for this family (there is no claimed-but-silent audio pipe here).
    fn pushes_events(&self, info: &HidDeviceInfo) -> bool {
        self.claims(info)
    }

    /// FAMILY EVENT VOCABULARY: `05 11 <state>` is the capacitive tap-mute push (state 0=live,
    /// 1=muted) — HARDWARE-CAPTURED on the Seiren V3 Mini (2026-07-08) and heuristically assumed to
    /// hold across the whole razer-audio family until a counterexample device disagrees. A def's own
    /// `[events]` table (`DeviceDef::event_for`) is checked FIRST by callers and OVERRIDES this per
    /// device — this is only the fallback when no def (or an empty/auto one) says otherwise.
    fn default_event_for(&self, report: &[u8]) -> Option<crate::registry::EventKind> {
        if report.len() >= 2 && report[0] == 0x05 && report[1] == 0x11 {
            Some(crate::registry::EventKind::MuteState)
        } else {
            None
        }
    }

    /// OVERRIDE that documents the proof, not a default fallthrough: the Seiren's tap-mute register is
    /// hardware-verified READ-ONLY (sensor-owned) — an EXHAUSTIVE getter-oracle sweep (all classes
    /// 0x00-0x3F, class 0x08 fully) plus user LED confirmation found NO command that writes it. Stays
    /// explicit `false` rather than relying on the trait default so a future reader of this impl sees
    /// the negative result was checked, not assumed.
    fn audio_mute_writable(&self) -> bool {
        false
    }

    /// The authoritative getter: class 0x08 id 0x88, hardware-verified on the Seiren V3 Mini
    /// (2026-07-08) — family-level, like [`Dialect::default_event_for`]'s push vocabulary. The reply
    /// body's `args[1]` (buffer offset 10) is the mute state (0 live / 1 muted); `args[0]` is a
    /// constant 0x01 selector, ignored here. `exec`'s `Err` (device asleep / link down) collapses to
    /// `None` — this is a best-effort UI seed, never a hard requirement.
    fn read_audio_mute(&self, t: &dyn Transport) -> Option<bool> {
        let args = self.exec(t, AUDIO_MUTE_READ_TX, 0x08, 0x88, 0x02, &[]).ok()?;
        Some(args[1] != 0)
    }
}

/// Build the razer-audio family's 64-byte request: report id [`AUDIO_REPORT_ID`] (razer_report's is
/// 0x00), the SAME field slots as [`frame`] (status/tx/size/class/id at buf[1]/[2]/[6]/[7]/[8] —
/// [`reply_status`]'s echo filter reads only those, so it is reused unchanged for this envelope), a
/// [`AUDIO_BODY_LEN`]-byte arg body at buf[9..62], and CRC = XOR(buf[2..=61]) at buf[62] (buf[63]
/// reserved). Args past the body are dropped, exactly as [`frame`] drops args past razer_report's
/// 80-byte body.
fn frame_audio(tx: u8, class: u8, id: u8, size: u8, args: &[u8]) -> [u8; AUDIO_BUF_LEN] {
    let mut b = [0u8; AUDIO_BUF_LEN];
    b[0] = AUDIO_REPORT_ID;
    b[2] = tx;
    b[6] = size;
    b[7] = class;
    b[8] = id;
    let n = args.len().min(AUDIO_BODY_LEN);
    b[9..9 + n].copy_from_slice(&args[..n]);
    b[62] = b[2..=61].iter().fold(0u8, |c, &x| c ^ x);
    b
}

/// Lift the razer-audio reply body (buf[9..62], [`AUDIO_BODY_LEN`] bytes) into the 80-byte arg shape
/// [`Dialect::exec`] promises callers — zero-padded past the bytes this shorter envelope carries.
fn args_from_audio_buf(b: &[u8; AUDIO_BUF_LEN]) -> [u8; 80] {
    let mut args = [0u8; 80];
    args[..AUDIO_BODY_LEN].copy_from_slice(&b[9..62]);
    args
}

/// The Logitech HID++ 2.0 family (dialect #2, wave 3 — spec-implemented, EXPERIMENTAL, no hardware
/// on this desk). See [`crate::hidpp`] for the wire vocabulary and the semantic-reshape contract.
static HIDPP: crate::hidpp::HidppDialect = crate::hidpp::HidppDialect;

/// The one razer-audio instance the registry hands out. A ZST, like [`RAZER`].
static RAZER_AUDIO: RazerAudioDialect = RazerAudioDialect;

/// The static dialect registry — slice order is claim order (razer first as the proven family, then
/// razer-audio, then hidpp; razer_report and razer-audio pipes are disjoint shapes on the same VID so
/// order between them is immaterial for claiming). No lazy_static/once_cell: a `static` slice over
/// the `static` items is a plain const initializer (the `&Dialect → &dyn Dialect` unsizing happens in
/// const context).
static DIALECTS: &[&dyn Dialect] = &[&RAZER, &RAZER_AUDIO, &HIDPP];

/// The first dialect that PUSHES device events on this collection ([`Dialect::pushes_events`]) —
/// slice order = claim order. `hidwatch::arm_new` arms a collection when this is `Some` OR a
/// registry def's own `event_pipe_matches` says yes; `hidwatch::decode` then resolves the actual
/// event via the def first (per-device override) and this dialect's `default_event_for` second
/// (family fallback). `None` when no registered family pushes anything on this shape.
pub fn event_dialect_for(info: &HidDeviceInfo) -> Option<&'static dyn Dialect> {
    dialects().iter().copied().find(|d| d.pushes_events(info))
}

/// The static dialect registry, in claim order.
pub fn dialects() -> &'static [&'static dyn Dialect] {
    DIALECTS
}

/// Resolve a dialect by its `DeviceDef.dialect` id. `None` for an id no family claims.
pub fn by_id(id: &str) -> Option<&'static dyn Dialect> {
    dialects().iter().copied().find(|d| d.id() == id)
}

/// The first dialect that `claims` this pipe (slice order = claim order). `None` when no family
/// speaks its wire shape. The app's adoption machinery routes on THIS instead of a hardcoded
/// vendor+report-len test, so a new dialect makes its devices adoptable with zero app changes.
pub fn claimed_by(info: &HidDeviceInfo) -> Option<&'static dyn Dialect> {
    dialects().iter().copied().find(|d| d.claims(info))
}

#[cfg(test)]
mod tests {
    use super::*; // brings Dialect, RazerDialect, frame, by_id, reply_status, Report, Transport, HidDeviceInfo, …
    use crate::transport::DevicePath;
    use std::sync::{Arc, Mutex};

    /// A razer_report device that RECORDS the exact request buffer and replies SUCCESS echoing the
    /// command's class/id — enough to pin the bytes the dialect puts on the wire AND prove the
    /// reply round-trips back through `reply_status`. Simplified from synth.rs's MockDevice.
    struct RecordingMock {
        last: Mutex<Option<[u8; BUF_LEN]>>,
    }

    impl RecordingMock {
        fn new() -> Self {
            RecordingMock {
                last: Mutex::new(None),
            }
        }
    }

    impl Transport for RecordingMock {
        fn set_feature(&self, buf: &[u8]) -> anyhow::Result<()> {
            let mut b = [0u8; BUF_LEN];
            let n = buf.len().min(BUF_LEN);
            b[..n].copy_from_slice(&buf[..n]);
            *self.last.lock().unwrap() = Some(b);
            Ok(())
        }
        fn get_feature(&self, buf: &mut [u8]) -> anyhow::Result<()> {
            let req = self.last.lock().unwrap().expect("a command was sent first");
            // Echo class/id (and tx/size) from the recorded request, status SUCCESS.
            let mut rep = Report::command(req[2], req[7], req[8], req[6]);
            rep.status = 0x02; // SUCCESS
            let out = rep.to_buf();
            let n = buf.len().min(BUF_LEN);
            buf[..n].copy_from_slice(&out[..n]);
            Ok(())
        }
    }

    /// The hard-coded byte golden for frame `(tx 0x1F, class 0x04, id 0x85, size 0x07, no args)`:
    /// report-id 0, status 0, tx at [2], size at [6], class at [7], id at [8], crc at [89]. The
    /// crc is XOR of the report body [3..=88] = 0x07 ^ 0x04 ^ 0x85 = 0x86. A regression in EITHER
    /// the dialect's `frame` OR `protocol::Report` is caught by pinning both against this.
    fn golden_frame() -> [u8; BUF_LEN] {
        let mut want = [0u8; BUF_LEN];
        want[2] = 0x1F; // transaction id
        want[6] = 0x07; // data size
        want[7] = 0x04; // class
        want[8] = 0x85; // id
        want[89] = 0x86; // crc
        want
    }

    #[test]
    fn frame_builder_matches_report_and_hardcoded_golden() {
        let via_frame = frame(0x1F, 0x04, 0x85, 0x07, &[]);
        let via_report = Report::command(0x1F, 0x04, 0x85, 0x07).to_buf();
        assert_eq!(
            via_frame, via_report,
            "the dialect's private frame builder must equal Report::command().to_buf()"
        );
        assert_eq!(
            via_frame,
            golden_frame(),
            "and both must equal the hand-computed byte golden"
        );
    }

    #[test]
    fn razer_exec_emits_golden_bytes_and_round_trips() {
        let mock = RecordingMock::new();
        // Route the exact raw parts Device passes today; the reply body is zero here.
        let out = RazerDialect
            .exec(&mock, 0x1F, 0x04, 0x85, 0x07, &[])
            .expect("mock replies SUCCESS");
        let sent = mock.last.lock().unwrap().expect("a command was sent");
        assert_eq!(
            sent,
            golden_frame(),
            "the request bytes the dialect emitted must be byte-identical to the golden frame"
        );
        assert_eq!(out, [0u8; 80], "reply body (80 args) round-trips");
        // The status echo filter itself: an echoing SUCCESS reply yields Success; a mismatch None.
        let mut reply = Report::command(0x1F, 0x04, 0x85, 0x07).to_buf();
        reply[1] = 0x02;
        assert_eq!(reply_status(&reply, 0x04, 0x85), Some(Status::Success));
        assert_eq!(reply_status(&reply, 0x04, 0x86), None, "class/id mismatch → no status");
    }

    #[test]
    fn razer_release_custody_emits_the_golden_mode_normal_frame() {
        let mock = RecordingMock::new();
        // def_with stamps transaction_id 31 (0x1F) — release_custody frames with the def's own tx.
        let def = def_with("razer", 0x0c, 0x01, 91);
        RazerDialect
            .release_custody(&mock, &def)
            .expect("mock replies SUCCESS to the mode-normal write");
        let sent = mock.last.lock().unwrap().expect("a command was sent");
        // device_mode -> NORMAL: tx 0x1F (def's), class 0x00, id 0x04, size 0x02, args [0x00, 0x00].
        // Byte-identical to the raw `writes::set_device_mode(d, 0x00)` teardown used to emit.
        let want = frame(0x1F, DEVICE_MODE_CLASS, DEVICE_MODE_SET_ID, 0x02, &[DEVICE_MODE_NORMAL, 0x00]);
        assert_eq!(
            sent, want,
            "release_custody must emit exactly the mode-normal frame"
        );
    }

    #[test]
    fn hidpp_release_custody_touches_the_wire_not_at_all() {
        // HID++ devices are NEVER in our custody (no device-mode concept) — the trait's default no-op
        // IS the correct release. A transport that panics on ANY I/O proves the hidpp dialect sends
        // nothing: a regression that ever framed a (razer-shaped) mode packet at HID++ hardware would
        // surface as a panic, not a silent wrong-bytes pass.
        struct PanicOnIo;
        impl Transport for PanicOnIo {
            fn set_feature(&self, _buf: &[u8]) -> anyhow::Result<()> {
                panic!("HID++ is never in our custody — release must touch the wire NOT AT ALL")
            }
            fn get_feature(&self, _buf: &mut [u8]) -> anyhow::Result<()> {
                panic!("HID++ release must read NOTHING")
            }
        }
        let def = def_with("hidpp", 0x00, 0x00, 0);
        crate::hidpp::HidppDialect
            .release_custody(&PanicOnIo, &def)
            .expect("the default no-op release is infallible");
    }

    #[test]
    fn razer_claims_only_its_signature() {
        let info = |vid: u16, feature_len: u16| HidDeviceInfo {
            vid,
            pid: 0,
            usage_page: 0,
            usage: 0,
            feature_len,
            input_len: 0,
            output_len: 0,
            path: DevicePath::from_str_for_tests("x"),
            product: String::new(),
        };
        assert!(RazerDialect.claims(&info(0x1532, 91)), "the razer_report signature");
        // The desk's real USB Sound Card shape — Razer VID but a 41-byte vendor pipe, NOT
        // razer_report — must NOT be claimed (it needs its own dialect, wave 3).
        assert!(!RazerDialect.claims(&info(0x1532, 41)), "41-byte audio sidecar pipe");
        // A non-Razer vendor with the right feature len is still not ours.
        assert!(!RazerDialect.claims(&info(0x046D, 91)), "Logitech VID");
    }

    /// Build a minimal DeviceDef with a given dialect tag + control triple, via TOML (the only
    /// public constructor). vendor_id 5426 = 0x1532; the rest is inert for matches_control.
    fn def_with(dialect: &str, usage_page: u16, usage: u16, feature_report_len: u16) -> crate::registry::DeviceDef {
        let text = format!(
            "name = \"T\"\ncodename = \"t\"\ndialect = \"{dialect}\"\nvendor_id = 5426\ntransaction_id = 31\n\
             [[modes]]\nname = \"default\"\nproduct_id = 1\n\
             [control_interface]\nusage_page = {usage_page}\nusage = {usage}\nfeature_report_len = {feature_report_len}\n\
             [commands]\n"
        );
        toml::from_str(&text).expect("minimal test def parses")
    }

    #[test]
    fn razer_matches_control_by_the_triple() {
        let def = def_with("razer", 0x0c, 0x01, 91);
        let pipe = |up: u16, us: u16, flen: u16| HidDeviceInfo {
            vid: RAZER_VID,
            pid: 0,
            usage_page: up,
            usage: us,
            feature_len: flen,
            input_len: 0,
            output_len: 0,
            path: DevicePath::from_str_for_tests("x"),
            product: String::new(),
        };
        assert!(def.matches_control(&pipe(0x0c, 0x01, 91)), "the def's exact triple = its control pipe");
        assert!(!def.matches_control(&pipe(0x0c, 0x01, 65)), "wrong feature_len = a different collection");
        assert!(!def.matches_control(&pipe(0x01, 0x02, 91)), "wrong usage_page/usage = a different collection");
    }

    #[test]
    fn unknown_dialect_def_matches_no_pipe_fail_closed() {
        // A def whose family we can't identify (typo / stale user-editable auto file) must select
        // NOTHING — even a byte-perfect triple — so it can never be opened into bytes we can't safely
        // frame (Finding 1's fail-closed clause, the selection-side sibling of Finding 2).
        let def = def_with("nope", 0x0c, 0x01, 91);
        let info = HidDeviceInfo {
            vid: RAZER_VID,
            pid: 0,
            usage_page: 0x0c,
            usage: 0x01,
            feature_len: 91,
            input_len: 0,
            output_len: 0,
            path: DevicePath::from_str_for_tests("x"),
            product: String::new(),
        };
        assert!(!def.matches_control(&info), "unknown dialect id → matches nothing at all");
    }

    #[test]
    fn by_id_resolves_registered_families() {
        assert!(by_id("razer").is_some());
        assert_eq!(by_id("razer").unwrap().id(), "razer");
        // Wave 3 registered hidpp — it resolves and its probe ADOPTS (spec-implemented,
        // hardware-unverified; see the hidpp module's EXPERIMENTAL banner).
        assert_eq!(by_id("hidpp").map(|d| d.id()), Some("hidpp"));
        assert!(by_id("nope").is_none());
    }

    #[test]
    fn interested_widens_to_vendor_but_claims_stays_pipe_precise() {
        let info = |vid: u16, feature_len: u16| HidDeviceInfo {
            vid,
            pid: 0,
            usage_page: 0,
            usage: 0,
            feature_len,
            input_len: 0,
            output_len: 0,
            path: DevicePath::from_str_for_tests("x"),
            product: String::new(),
        };
        // The desk's real audio sidecar: Razer VID, but a 41-byte pipe (NOT razer_report). The
        // dialect is INTERESTED (our vendor's hardware) yet does NOT CLAIM it (no protocol we
        // speak) — exactly the split that populates the unclaimed ledger.
        assert!(RazerDialect.interested(&info(0x1532, 41)), "Razer vendor → interested");
        assert!(!RazerDialect.claims(&info(0x1532, 41)), "41-byte pipe → not razer_report → unclaimed");
        // A claimable control pipe is both.
        assert!(RazerDialect.interested(&info(0x1532, 91)));
        assert!(RazerDialect.claims(&info(0x1532, 91)));
        // A foreign vendor is neither, whatever the report shape.
        assert!(!RazerDialect.interested(&info(0x046D, 91)), "Logitech VID → not our vendor");
        assert!(!RazerDialect.interested(&info(0x046D, 7)), "Logitech VID → not interested regardless");
    }

    #[test]
    fn claimed_by_routes_the_control_pipe_only() {
        let info = |vid: u16, feature_len: u16| HidDeviceInfo {
            vid,
            pid: 0,
            usage_page: 0,
            usage: 0,
            feature_len,
            input_len: 0,
            output_len: 0,
            path: DevicePath::from_str_for_tests("x"),
            product: String::new(),
        };
        assert_eq!(claimed_by(&info(0x1532, 91)).map(|d| d.id()), Some("razer"));
        assert!(claimed_by(&info(0x1532, 41)).is_none(), "interested but unclaimed");
        assert!(claimed_by(&info(0x046D, 91)).is_none(), "foreign vendor");
    }

    /// A pipe shaped like `(usage_page, usage, feature_len)` on the given vendor — the builder every
    /// razer-audio test below shares.
    fn audio_pipe(vid: u16, usage_page: u16, usage: u16, feature_len: u16) -> HidDeviceInfo {
        HidDeviceInfo {
            vid,
            pid: 0x056A,
            usage_page,
            usage,
            feature_len,
            input_len: 64,
            output_len: 0,
            path: DevicePath::from_str_for_tests("x"),
            product: String::new(),
        }
    }

    #[test]
    fn razer_audio_claims_only_its_signature() {
        // The right shape: Razer VID, Consumer-Control usage pair, 64-byte feature report.
        assert!(RazerAudioDialect.claims(&audio_pipe(RAZER_VID, 0x000C, 0x0001, 64)), "the razer-audio signature");
        // The razer_report mouse/keyboard shape (91-byte feature report) is NOT this family's pipe.
        assert!(!RazerAudioDialect.claims(&audio_pipe(RAZER_VID, 0x000C, 0x0001, 91)), "91-byte pipe is razer_report's, not razer-audio's");
        // Right vendor + length, wrong usage — a different collection entirely.
        assert!(!RazerAudioDialect.claims(&audio_pipe(RAZER_VID, 0x0001, 0x0002, 64)), "wrong usage pair");
        // A foreign vendor with the identical shape is still not ours.
        assert!(!RazerAudioDialect.claims(&audio_pipe(0x046D, 0x000C, 0x0001, 64)), "Logitech VID");
        // And RazerDialect must NOT claim the audio shape either — the two families stay disjoint.
        assert!(!RazerDialect.claims(&audio_pipe(RAZER_VID, 0x000C, 0x0001, 64)), "razer_report never claims the audio pipe");
    }

    #[test]
    fn frame_audio_emits_golden_bytes_with_the_64_byte_crc_span() {
        // tx 0x1F, class 0x00, id 0x82 (serial getter), size 0x16, no args — the exact request this
        // dialect's `probe`/`exec` would frame for the Seiren's serial getter.
        let got = frame_audio(0x1F, 0x00, 0x82, 0x16, &[]);
        let mut want = [0u8; AUDIO_BUF_LEN];
        want[0] = 0x07; // report id — NOT razer_report's 0x00
        want[2] = 0x1F; // transaction id
        want[6] = 0x16; // data size
        want[7] = 0x00; // class
        want[8] = 0x82; // id
        want[62] = (0x1F ^ 0x16) ^ 0x82; // CRC = XOR(buf[2..=61]); [2],[6],[7],[8] are nonzero there
        assert_eq!(got, want, "frame_audio must match the hand-computed 64-byte golden");
        assert_eq!(got.len(), 64, "the razer-audio envelope is 64 bytes, not razer_report's 91");
    }

    #[test]
    fn razer_audio_exec_round_trips_through_the_shared_echo_filter() {
        // A 64-byte mock that echoes class/id/size with status SUCCESS — proves `reply_status` (built
        // for the 91-byte envelope) reads this shorter envelope's offsets 1/7/8 correctly.
        struct AudioMock {
            last: Mutex<Option<[u8; AUDIO_BUF_LEN]>>,
        }
        impl Transport for AudioMock {
            fn set_feature(&self, buf: &[u8]) -> anyhow::Result<()> {
                let mut b = [0u8; AUDIO_BUF_LEN];
                let n = buf.len().min(AUDIO_BUF_LEN);
                b[..n].copy_from_slice(&buf[..n]);
                *self.last.lock().unwrap() = Some(b);
                Ok(())
            }
            fn get_feature(&self, buf: &mut [u8]) -> anyhow::Result<()> {
                let req = self.last.lock().unwrap().expect("a command was sent first");
                let mut rep = [0u8; AUDIO_BUF_LEN];
                rep[1] = 0x02; // Success
                rep[7] = req[7]; // echo class
                rep[8] = req[8]; // echo id
                let n = buf.len().min(AUDIO_BUF_LEN);
                buf[..n].copy_from_slice(&rep[..n]);
                Ok(())
            }
        }
        let mock = AudioMock { last: Mutex::new(None) };
        let out = RazerAudioDialect
            .exec(&mock, 0x1F, 0x00, 0x82, 0x16, &[])
            .expect("mock replies SUCCESS");
        assert_eq!(out, [0u8; 80], "reply body round-trips (mock sent all-zero args)");
        let sent = mock.last.lock().unwrap().expect("a command was sent");
        assert_eq!(sent, frame_audio(0x1F, 0x00, 0x82, 0x16, &[]), "request bytes match frame_audio");
    }

    #[test]
    fn razer_audio_pushes_and_maps_the_family_wide_mute_vocabulary() {
        let shape = audio_pipe(RAZER_VID, 0x000C, 0x0001, 64);
        assert!(RazerAudioDialect.pushes_events(&shape), "a claimed pipe also pushes events");
        assert!(!RazerAudioDialect.pushes_events(&audio_pipe(RAZER_VID, 0x000C, 0x0001, 91)), "an unclaimed pipe pushes nothing");
        // `05 11` -> MuteState, any other lead pair -> None (default_event_for is the FAMILY fallback,
        // checked only after a def's own event_for finds nothing).
        assert_eq!(
            RazerAudioDialect.default_event_for(&[0x05, 0x11, 0x01]),
            Some(crate::registry::EventKind::MuteState)
        );
        assert_eq!(RazerAudioDialect.default_event_for(&[0x05, 0x11]), Some(crate::registry::EventKind::MuteState), "2 lead bytes is enough");
        assert_eq!(RazerAudioDialect.default_event_for(&[0x05, 0x02, 0x00]), None, "a different sub-kind is not mute");
        assert_eq!(RazerAudioDialect.default_event_for(&[0x05]), None, "too short to carry the lead pair");
        // RazerDialect / HidppDialect never grew this vocabulary — the default `None`/`false` stays.
        assert!(!RazerDialect.pushes_events(&shape));
        assert_eq!(RazerDialect.default_event_for(&[0x05, 0x11, 0x01]), None);
    }

    #[test]
    fn razer_audio_is_read_only_never_writable() {
        // The WRITE facet's whole point: an EXHAUSTIVE getter-oracle sweep found no setter for the
        // Seiren's tap-mute register, so this must stay `false` — an explicit override of the proof,
        // not a silent default.
        assert!(!RazerAudioDialect.audio_mute_writable());
    }

    #[test]
    fn read_audio_mute_none_when_exec_fails() {
        // A transport that fails every I/O call (device asleep / unplugged) must collapse to `None`,
        // never a fabricated true/false.
        struct AlwaysFail;
        impl Transport for AlwaysFail {
            fn set_feature(&self, _buf: &[u8]) -> anyhow::Result<()> {
                bail!("no device")
            }
            fn get_feature(&self, _buf: &mut [u8]) -> anyhow::Result<()> {
                bail!("no device")
            }
        }
        assert_eq!(RazerAudioDialect.read_audio_mute(&AlwaysFail), None);
    }

    #[test]
    fn read_audio_mute_parses_args1_as_the_state_byte() {
        // A mock that answers the 0x08/0x88 getter with args[0]=0x01 (the constant selector, ignored)
        // and args[1]=the mute state — proves `read_audio_mute` reads offset 1, not 0.
        struct MuteMock {
            last: Mutex<Option<[u8; AUDIO_BUF_LEN]>>,
            state: u8,
        }
        impl Transport for MuteMock {
            fn set_feature(&self, buf: &[u8]) -> anyhow::Result<()> {
                let mut b = [0u8; AUDIO_BUF_LEN];
                let n = buf.len().min(AUDIO_BUF_LEN);
                b[..n].copy_from_slice(&buf[..n]);
                *self.last.lock().unwrap() = Some(b);
                Ok(())
            }
            fn get_feature(&self, buf: &mut [u8]) -> anyhow::Result<()> {
                let req = self.last.lock().unwrap().expect("a command was sent first");
                let mut rep = [0u8; AUDIO_BUF_LEN];
                rep[1] = 0x02; // Success
                rep[7] = req[7]; // echo class
                rep[8] = req[8]; // echo id
                rep[9] = 0x01; // args[0]: the constant selector, ignored by read_audio_mute
                rep[10] = self.state; // args[1]: the mute state byte
                let n = buf.len().min(AUDIO_BUF_LEN);
                buf[..n].copy_from_slice(&rep[..n]);
                Ok(())
            }
        }
        let live = MuteMock { last: Mutex::new(None), state: 0 };
        assert_eq!(RazerAudioDialect.read_audio_mute(&live), Some(false), "state 0 = live");
        let muted = MuteMock { last: Mutex::new(None), state: 1 };
        assert_eq!(RazerAudioDialect.read_audio_mute(&muted), Some(true), "state 1 = muted");
    }

    #[test]
    fn event_dialect_for_resolves_the_pushing_family_only() {
        assert_eq!(
            event_dialect_for(&audio_pipe(RAZER_VID, 0x000C, 0x0001, 64)).map(|d| d.id()),
            Some("razer-audio")
        );
        assert!(event_dialect_for(&audio_pipe(RAZER_VID, 0x000C, 0x0001, 91)).is_none(), "razer_report's shape pushes nothing family-wide");
        assert!(event_dialect_for(&audio_pipe(0x046D, 0x000C, 0x0001, 64)).is_none(), "foreign vendor");
    }

    #[test]
    fn razer_audio_probe_yields_an_empty_inoperable_def() {
        // No transport I/O at all — `probe` never sends a byte (see its doc: the family parrots
        // Success+echo for unknown headers, so no probed command would be trustworthy evidence).
        struct PanicOnIo;
        impl Transport for PanicOnIo {
            fn set_feature(&self, _buf: &[u8]) -> anyhow::Result<()> {
                panic!("razer-audio probe must never write to the wire")
            }
            fn get_feature(&self, _buf: &mut [u8]) -> anyhow::Result<()> {
                panic!("razer-audio probe must never read the wire")
            }
        }
        let ctx = crate::synth::SynthCtx {
            vid: RAZER_VID,
            pid: 0x056A,
            usage_page: 0x000C,
            usage: 0x0001,
            feature_len: 64,
            product: "Razer Seiren V3 Mini".into(),
        };
        let s = RazerAudioDialect
            .probe(&PanicOnIo, &ctx)
            .expect("a claimed pipe always synthesizes Some, even with an empty command map");
        assert_eq!(s.def.dialect, "razer-audio");
        assert_eq!(s.def.name, "Razer Seiren V3 Mini");
        assert!(s.def.commands.is_empty(), "no command survives — none was ever probed");
        assert!(s.def.lighting.is_none());
        assert!(!s.def.is_operable(), "an empty-commands, no-lighting def must never grow a device row");
        // Round-trips through the same emitter/loader every other synthesis uses.
        let text = crate::synth::emit_toml(&s);
        let mut parsed: crate::registry::DeviceDef =
            toml::from_str(&text).unwrap_or_else(|e| panic!("emitted razer-audio TOML must parse: {e}\n---\n{text}"));
        parsed.origin = s.def.origin.clone();
        assert_eq!(parsed, s.def, "emit -> parse must be lossless for a razer-audio def");
        assert!(!parsed.is_operable(), "the reloaded def is still honestly inoperable");
    }

    #[test]
    fn razer_audio_probe_falls_back_to_a_pid_name_when_product_is_empty() {
        struct PanicOnIo;
        impl Transport for PanicOnIo {
            fn set_feature(&self, _buf: &[u8]) -> anyhow::Result<()> {
                panic!("no I/O expected")
            }
            fn get_feature(&self, _buf: &mut [u8]) -> anyhow::Result<()> {
                panic!("no I/O expected")
            }
        }
        let ctx = crate::synth::SynthCtx {
            vid: RAZER_VID,
            pid: 0x056A,
            usage_page: 0x000C,
            usage: 0x0001,
            feature_len: 64,
            product: String::new(),
        };
        let s = RazerAudioDialect.probe(&PanicOnIo, &ctx).unwrap();
        assert_eq!(s.def.name, "Razer audio device 056a");
    }

    /// What the ONE simulated firmware control pipe currently holds — "whatever the LAST
    /// `set_feature` wrote" is exactly the real hardware's behavior: the pipe has
    /// no per-caller memory, so a `get_feature` from ANY handle echoes whoever wrote most recently.
    /// `violations` is incremented by [`SharedPipe::get_feature`] whenever a THREAD's own poll
    /// observes a class/id different from the one IT most recently sent — i.e. another thread's
    /// `set_feature` landed in between, a cross-read.
    struct PipeState {
        class: u8,
        id: u8,
        violations: usize,
    }

    /// A fake `Transport` that simulates the shared control pipe TWO real HID handles opened on the
    /// same `DevicePath` would present. Two `SharedPipe` values built via [`SharedPipe::second_handle`]
    /// share the SAME inner `state` AND the SAME `wire` lock — exactly what two `WinHid::open` calls
    /// on one path would produce through `transport::wire_lock_for`. `set_feature` OVERWRITES the
    /// shared state (with a short sleep that widens the interleave window a real 10ms poll cadence
    /// would otherwise mostly hide); `get_feature` answers Success echoing whatever is CURRENTLY
    /// there — faithfully modeling the last-set-wins cross-read behavior.
    struct SharedPipe {
        state: Arc<Mutex<PipeState>>,
        wire: Arc<crate::transport::WireLock>,
    }

    impl SharedPipe {
        fn new() -> Self {
            SharedPipe {
                state: Arc::new(Mutex::new(PipeState { class: 0, id: 0, violations: 0 })),
                wire: Arc::new(crate::transport::WireLock::new_local()),
            }
        }

        /// A second handle onto the SAME simulated pipe: same `state`, same `wire` — the fake's
        /// analogue of a second in-process `WinHid::open` on the identical `DevicePath`.
        fn second_handle(&self) -> SharedPipe {
            SharedPipe {
                state: self.state.clone(),
                wire: self.wire.clone(),
            }
        }
    }

    thread_local! {
        // The (class, id) THIS thread most recently sent — compared against the shared pipe state
        // inside `get_feature` to detect a cross-read from the OTHER thread's conversation.
        static LAST_SENT: std::cell::Cell<(u8, u8)> = const { std::cell::Cell::new((0, 0)) };
    }

    impl Transport for SharedPipe {
        fn set_feature(&self, buf: &[u8]) -> anyhow::Result<()> {
            let (class, id) = (buf[7], buf[8]);
            LAST_SENT.with(|c| c.set((class, id)));
            {
                let mut st = self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                st.class = class;
                st.id = id;
            }
            // Widen the interleave window: WITHOUT the wire lock, this is exactly the gap another
            // thread's own `set_feature` would land in and overwrite `state` out from under us.
            std::thread::sleep(std::time::Duration::from_micros(300));
            Ok(())
        }

        fn get_feature(&self, buf: &mut [u8]) -> anyhow::Result<()> {
            let (want_class, want_id) = LAST_SENT.with(|c| c.get());
            let (cur_class, cur_id) = {
                let st = self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                (st.class, st.id)
            };
            if (cur_class, cur_id) != (want_class, want_id) {
                self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner).violations += 1;
            }
            // Success, echoing whatever the pipe CURRENTLY holds — the last-set-wins cross-read.
            let mut rep = Report::command(0x1F, cur_class, cur_id, 0);
            rep.status = 0x02; // Success
            let out = rep.to_buf();
            let n = buf.len().min(BUF_LEN);
            buf[..n].copy_from_slice(&out[..n]);
            Ok(())
        }

        fn wire_lock(&self) -> Option<Arc<crate::transport::WireLock>> {
            // Both `SharedPipe` handles built from one `second_handle()` call return the IDENTICAL
            // Arc — precisely how two real `WinHid` opens on the same `DevicePath` behave.
            Some(self.wire.clone())
        }
    }

    #[test]
    fn two_handles_never_cross_read_replies_on_one_pipe() {
        // INVERSE CONTROL (documented, not asserted): comment out `RazerDialect::exec`'s
        // `t.wire_lock()` guard and this SAME fake demonstrably cross-reads — with two threads
        // racing `SharedPipe::set_feature`/`get_feature` unguarded, a `get_feature` between them
        // routinely echoes the OTHER thread's class/id (`violations` climbs well above zero). The
        // lock exercised below is what keeps `violations` at zero; it is load-bearing, not
        // incidental scaffolding.
        let pipe = SharedPipe::new();
        let h1 = pipe.second_handle();
        let h2 = pipe.second_handle();

        // Two threads, ~50 conversations each, DIFFERENT (class, id) pairs per thread so a
        // cross-read is distinguishable from a same-thread repeat.
        let t1 = std::thread::spawn(move || {
            for i in 0..50u8 {
                let out = RazerDialect.exec(&h1, 0x1F, 0x10, i, 0x02, &[]);
                assert!(out.is_ok(), "thread 1's exec must succeed under the wire lock");
            }
        });
        let t2 = std::thread::spawn(move || {
            for i in 0..50u8 {
                let out = RazerDialect.exec(&h2, 0x1F, 0x20, i, 0x02, &[]);
                assert!(out.is_ok(), "thread 2's exec must succeed under the wire lock");
            }
        });
        t1.join().expect("thread 1 must not panic");
        t2.join().expect("thread 2 must not panic");

        assert_eq!(
            pipe.state.lock().unwrap().violations,
            0,
            "no thread's get_feature may ever observe the OTHER thread's class/id mid-conversation \
             — the wire lock must keep every set→poll conversation atomic"
        );
    }
}
