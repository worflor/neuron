//! Protocol FAMILIES as a pluggable seam. Everything above the wire stays semantic
//! (`Capability::*`, the registry, FEEL/LIGHTING, profiles, adoption UX); everything
//! wire-shaped — bus signature, framing, status semantics, exec discipline — lives behind a
//! [`Dialect`]. `razer_report` is dialect #1, its production exec loops MOVED here verbatim from
//! `device.rs` so every existing device's traffic is byte-identical (pinned by the frame goldens
//! below). Semantics live above (Capability — a HID++ mouse's 0x1000 battery and a Razer mouse's
//! 0x07/0x80 battery are the SAME `Capability::Battery` to everything above this line); bytes live
//! here. See docs/DIALECT-RND.md for the wave plan.

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

/// The Logitech HID++ 2.0 family (dialect #2, wave 3 — spec-implemented, EXPERIMENTAL, no hardware
/// on this desk). See [`crate::hidpp`] for the wire vocabulary and the semantic-reshape contract.
static HIDPP: crate::hidpp::HidppDialect = crate::hidpp::HidppDialect;

/// The static dialect registry — slice order is claim order (razer first, then hidpp; the two
/// vendors are disjoint so order is immaterial for claiming, but razer stays first as the proven
/// family). No lazy_static/once_cell: a `static` slice over the `static` items is a plain const
/// initializer (the `&Dialect → &dyn Dialect` unsizing happens in const context).
static DIALECTS: &[&dyn Dialect] = &[&RAZER, &HIDPP];

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
    use std::sync::Mutex;

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
}
