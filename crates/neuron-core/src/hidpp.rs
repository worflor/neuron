//! Logitech HID++ 2.0 — dialect #2 (DIALECT-RND wave 3). **EXPERIMENTAL, spec-implemented, ZERO
//! hardware verification.** Every byte layout here is transcribed from the libratbag / Solaar /
//! Logitech `cpg-docs` documentation (URLs in the frame/reshape comments below), NOT observed on a
//! wire — there is no Logitech device on the development desk. The dialect CLAIMS its pipes, is
//! INTERESTED at vendor granularity, and now ADOPTS: [`HidppDialect::probe`] is REAL — it pings
//! IRoot, enumerates the battery/DPI features, and synthesizes a [`crate::synth::Synthesis`] via the
//! `pub(crate)` [`crate::synth::Synthesis::from_probe`] mint seam. What is still missing is HARDWARE:
//! the emitted defs land as `devices/auto/hidpp-<pid>.toml` and are honest-but-UNVERIFIED — the wire
//! layouts they encode have never round-tripped against a real Logitech device. Anyone with the
//! hardware runs `neuron adopt` (or the read-only [`probe_report`] diagnostic) to confirm the spec;
//! the READ-only getters risk nothing, and no setters are synthesized (see [`HidppDialect::probe`]).
//!
//! ## Second wire surface
//! HID++ does NOT use feature reports. A request rides an OUTPUT report (`Transport::write_output`
//! → `WriteFile`) and each reply arrives as an INPUT report (`Transport::read_input` → `ReadFile`).
//! That is the whole reason [`crate::transport::Transport`] grew those two methods.
//!
//! ## The claiming contract (what [`HidppDialect::claims`] PROMISES)
//! `claims()` is a PROMISE, not a guess: "adopt me and I can complete my request/reply round-trip on
//! THIS pipe." [`HidppDialect::probe`] and [`HidppDialect::exec`] UNCONDITIONALLY `write_output` a
//! request and THEN `read_input` its reply, addressed at the one endpoint this dialect knows
//! ([`DEVICE_INDEX`] `0xFF`, direct-attached). So a claimable pipe must satisfy BOTH halves of that
//! flow: a coherent BIDIRECTIONAL HID++ report shape (an output report of a HID++ class AND an input
//! report able to carry the reply) on a DIRECT-attached device (not a receiver — receivers answer on
//! HID++ 1.0 registers / pairing slots this wave can't address). Anything the dialect merely
//! RECOGNIZES but cannot yet drive — a one-direction collection, a receiver, a non-HID++ Logitech
//! pipe — belongs to [`HidppDialect::interested`], NEVER to `claims`: `interested` lands it on the
//! unclaimed-ledger footnote as honest inventory ("logitech vendor pipe, no speakable path yet"),
//! whereas a wrongly-claimed pipe that can't answer surfaces to the user as a 3-strike "unresponsive"
//! device row — pinned to a device index the code itself would have sent to the wrong endpoint. Claim
//! only what you can round-trip; recognize the rest.
//!
//! ## Report shapes (confirmed: cpg-docs `hidpp20/README.rst`)
//! Two frames, identical field layout, different lengths:
//! ```text
//!   byte:  0          1             2              3                    4 .. N
//!          report id  device index  feature index  (function<<4)|swId   parameters
//!   short  0x10       ..            ..             ..                   3 bytes  (total 7)
//!   long   0x11       ..            ..             ..                   16 bytes (total 20)
//! ```
//! Function occupies the high nibble of byte 3, softwareId the low nibble; a reply echoes the
//! softwareId so a client can tell its own replies apart. We frame everything as LONG reports and
//! use a fixed [`SW_ID`] nibble. HID++ 2.0 is big-endian for multi-byte values.
//!
//! ## The semantic reply contract (the load-bearing design point)
//! Everything above the dialect line is dialect-BLIND: `capability::battery_percent` reads
//! `args[1]` as a 0..255 raw level and scales it `*100/255`; `capability::dpi` reads `args[1..5]`
//! as big-endian X_hi,X_lo,Y_hi,Y_lo. Those decoders must keep working unchanged for a HID++ mouse.
//! So a dialect's `exec` RESHAPES its raw reply into exactly the arg layout those decoders expect —
//! HID++ returns battery as a 0..100 PERCENTAGE, we scale it back up into the 0..255 space and land
//! it at `args[1]`; HID++ returns a single big-endian DPI, we mirror it onto the X and Y pairs at
//! `args[1..5]`. The command NAMES in the def are the contract keys (`battery_level`, `dpi`), the
//! same names the razer defs use, so the capability layer never learns which family answered.
//!
//! ## Dialect-private spec encoding: the reshape TAG (analogous to razer's varstore prefix bytes)
//! `exec` only receives the raw command spec bytes (class/id/size/args) — it can't know that
//! feature 0x1000-function-0 means "battery". So each HID++ command spec reserves `args[0]` as a
//! RESHAPE TAG owned by this dialect: [`RESHAPE_NONE`] (raw payload, left-aligned), [`RESHAPE_BATTERY`]
//! (percentage→0..255 at args[1]), [`RESHAPE_DPI`] (single DPI mirrored to X/Y at args[1..5]). `exec`
//! STRIPS the tag before building the wire parameters and APPLIES it to the reply. Data stays data;
//! the tag is dialect vocabulary, invisible above the wire. [`HidppDialect::probe`] emits these tags
//! into the synthesized command specs (battery ⇒ [`RESHAPE_BATTERY`], DPI ⇒ [`RESHAPE_DPI`]).

use crate::dialect::Dialect;
use crate::transport::{HidDeviceInfo, Transport};
use anyhow::{bail, Result};

/// Logitech's USB vendor id.
pub const HIDPP_VID: u16 = 0x046D;

/// Known Logitech USB RECEIVER product ids (Unifying / Bolt / Nano / Lightspeed / EX100 families).
/// A receiver is NOT a claimable HID++ 2.0 endpoint: the receiver ITSELF at [`DEVICE_INDEX`] `0xFF`
/// speaks HID++ 1.0 REGISTERS (not the 2.0 feature protocol [`HidppDialect::probe`] pings), and its
/// PAIRED devices answer on pairing SLOTS `1..=6` — addressing this dialect hard-codes away (see
/// [`DEVICE_INDEX`]). Until the receiver device-connection enumeration lands (DIALECT-RND HID++
/// wave), these pids are still recognized by [`HidppDialect::interested`] but never CLAIMED, so a
/// receiver surfaces as unclaimed inventory instead of a mis-addressed 3-strike "unresponsive" row.
/// List transcribed from Solaar `lib/logitech_receiver/base_usb.py` (the canonical VID-0x046D
/// receiver enumeration; cross-checked against libratbag). `claims()` already gates on the VID, so an
/// OEM-rebranded pid that is not truly a 0x046D device simply never matches — harmless.
const RECEIVER_PIDS: &[u16] = &[
    0xC517, // EX100 27 MHz receiver
    // Nano receivers (incl. the "advanced" 0xC52F and Dell/Lenovo rebrands)
    0xC518, 0xC51A, 0xC51B, 0xC521, 0xC525, 0xC526, 0xC52E, 0xC52F, 0xC531, 0xC534, 0xC535, 0xC537, 0x6042,
    0xC52B, 0xC532, // Unifying receivers
    0xC548, // Bolt receiver
    // Lightspeed receivers
    0xC539, 0xC53A, 0xC53D, 0xC53F, 0xC541, 0xC545, 0xC547, 0xC54D,
];

/// Is `pid` one of the [`RECEIVER_PIDS`]? A receiver is interested-but-never-claimed (see the
/// claiming-contract doc): it has no direct-attached `0xFF` HID++ 2.0 endpoint to round-trip against.
fn is_receiver_pid(pid: u16) -> bool {
    RECEIVER_PIDS.contains(&pid)
}

const REPORT_ID_SHORT: u8 = 0x10; // 7-byte short report
const REPORT_ID_LONG: u8 = 0x11; // 20-byte long report
const SHORT_LEN: u16 = 7;
const LONG_LEN: usize = 20;

/// Device index for every framed request. `0xFF` addresses a DIRECT-attached (corded) HID++ 2.0
/// device per spec (cpg-docs). This is CORRECT BY CONSTRUCTION for every CLAIMABLE pipe — not a
/// documented wrong-for-wireless simplification anymore: `claims()` now excludes [`RECEIVER_PIDS`], so
/// the only pipes this dialect adopts are direct-attached, and `0xFF` is exactly their endpoint. A
/// receiver-paired WIRELESS device answers on a pairing slot (`1..=6`) resolved via the receiver's
/// device-connection enumeration — but such pipes are no longer claimed (they sit on the
/// [`HidppDialect::interested`] ledger), so no claimable request is ever mis-addressed. Slot
/// addressing lands with the receiver-enumeration work in a later DIALECT-RND HID++ wave.
const DEVICE_INDEX: u8 = 0xFF;

/// Our fixed softwareId nibble (byte 3 low nibble). Any value 1..=15 works; a reply echoes it so we
/// can match our own replies. `0x0A` is arbitrary-but-distinctive (matches the `0x1A` byte-3 golden
/// for a function-1 request: `(1<<4)|0x0A`).
const SW_ID: u8 = 0x0A;

// IRoot (feature 0x0000) — confirmed: cpg-docs `hidpp20/features/0x0000-IRoot.rst`.
const IROOT_FEATURE: u8 = 0x00;
const IROOT_FN_GET_FEATURE: u8 = 0x00; // getFeature(featureId) → featureIndex
const IROOT_FN_GET_PROTOCOL: u8 = 0x01; // getProtocolVersion(pingData) → major,minor,pingEcho

/// A distinctive ping token echoed by getProtocolVersion — lets [`probe_report`] confirm the reply
/// is genuinely ours and the device is HID++ 2.0 (a 1.0 device would not echo it the same way).
const PING_TOKEN: u8 = 0x5A;

// Error markers in the feature-index slot (byte 2) — confirmed: Linux `hid-logitech-hidpp.c`
// (`HIDPP_ERROR` 0x8f / `HIDPP20_ERROR` 0xff). For BOTH, the error code sits at byte 5.
const ERR_HIDPP10: u8 = 0x8F;
const ERR_HIDPP20: u8 = 0xFF;

/// Reshape tags — the dialect-private `args[0]` vocabulary (see module doc).
pub const RESHAPE_NONE: u8 = 0x00;
pub const RESHAPE_BATTERY: u8 = 0x01;
pub const RESHAPE_DPI: u8 = 0x02;

/// Feature ids the probe looks for (documented HID++ 2.0 feature numbers).
pub const FEATURE_BATTERY: u16 = 0x1000; // BatteryLevelStatus
pub const FEATURE_ADJUSTABLE_DPI: u16 = 0x2201; // AdjustableDPI

/// Function ids WITHIN those features, baked into the synthesized command specs as `spec_id` (the
/// razer `id` slot). Function 0 of 0x1000 is getBatteryLevelStatus; getSensorDpi is the read on
/// 0x2201 — matched to the [`reshape`]/`exec` golden tests' `func_swid(0x02)` DPI convention.
const FN_GET_BATTERY_STATUS: u8 = 0x00; // 0x1000 getBatteryLevelStatus → discharge % at payload[0]
const FN_GET_SENSOR_DPI: u8 = 0x02; // 0x2201 getSensorDpi → single big-endian resolution

/// Build a 20-byte LONG request. `params` past the 16-byte body is dropped (a long report holds 16).
fn build_long(feature_idx: u8, fn_id: u8, params: &[u8]) -> [u8; LONG_LEN] {
    let mut r = [0u8; LONG_LEN];
    r[0] = REPORT_ID_LONG;
    r[1] = DEVICE_INDEX;
    r[2] = feature_idx;
    r[3] = (fn_id << 4) | (SW_ID & 0x0F);
    for (i, b) in params.iter().enumerate() {
        if 4 + i < LONG_LEN {
            r[4 + i] = *b;
        }
    }
    r
}

/// The byte-3 value a reply must echo to be ours: our function in the high nibble, our softwareId low.
fn func_swid(fn_id: u8) -> u8 {
    (fn_id << 4) | (SW_ID & 0x0F)
}

/// Human name for a HID++ 2.0 error code — confirmed: cpg-docs error-code enumeration.
fn err_name(code: u8) -> &'static str {
    match code {
        0x00 => "NoError",
        0x01 => "Unknown",
        0x02 => "InvalidArgument",
        0x03 => "OutOfRange",
        0x04 => "HardwareError",
        0x05 => "LogitechInternal",
        0x06 => "InvalidFeatureIndex",
        0x07 => "InvalidFunctionId",
        0x08 => "Busy",
        0x09 => "Unsupported",
        _ => "unspecified",
    }
}

/// Is `buf` an error report for our in-flight request? The feature-index slot carries `0x8F` (1.0)
/// or `0xFF` (2.0). We drive one request at a time on a dedicated handle, so any error bearing our
/// device index in the drain window is ours. Returns the error code (byte 5) when so.
fn error_code(buf: &[u8]) -> Option<u8> {
    if buf.len() >= 6 && buf[1] == DEVICE_INDEX && (buf[2] == ERR_HIDPP10 || buf[2] == ERR_HIDPP20) {
        Some(buf[5])
    } else {
        None
    }
}

/// Does `buf` echo a normal reply to `(feature_idx, fn_id)` for us? (report id short/long, our
/// device index, echoed feature index, echoed function|softwareId.)
fn matches_reply(buf: &[u8], feature_idx: u8, fn_id: u8) -> bool {
    buf.len() >= 4
        && (buf[0] == REPORT_ID_LONG || buf[0] == REPORT_ID_SHORT)
        && buf[1] == DEVICE_INDEX
        && buf[2] == feature_idx
        && buf[3] == func_swid(fn_id)
}

/// Drain input reports (up to ~10 reads / ~1s) for the NORMAL reply to `(feature_idx, fn_id)`.
/// `None` if an error report or nothing arrives — the diagnostic caller ([`probe_report`]) treats
/// both as "feature absent / didn't answer". `exec` does its OWN loop so it can surface the error
/// code as an `anyhow` error rather than a bare `None`.
fn recv_matching(t: &dyn Transport, feature_idx: u8, fn_id: u8) -> Option<[u8; LONG_LEN]> {
    for _ in 0..10 {
        let mut buf = [0u8; LONG_LEN];
        let n = match t.read_input(&mut buf, 100) {
            Ok(n) => n,
            Err(_) => continue, // nothing in this window; keep draining
        };
        let got = &buf[..n.min(LONG_LEN)];
        if error_code(got).is_some() {
            return None;
        }
        if matches_reply(got, feature_idx, fn_id) {
            return Some(buf);
        }
        // else: a reply for a different request/softwareId — keep draining.
    }
    None
}

/// Reshape a raw 16-byte function payload into the 80-byte arg layout the semantic decoders expect.
/// See the module doc's "semantic reply contract". Pure — unit-tested against the documented offsets.
fn reshape(tag: u8, payload: &[u8]) -> [u8; 80] {
    let mut out = [0u8; 80];
    match tag {
        RESHAPE_BATTERY => {
            // 0x1000 getBatteryLevelStatus payload[0] = batteryDischargeLevel, a 0..100 PERCENTAGE
            // (cpg-docs / lekensteyn x1000; 0 = unknown). `capability::battery_percent` reads
            // args[1] as a 0..255 raw level and computes (raw*100+127)/255, so scale the percentage
            // UP into that 0..255 space and land it at args[1] (round-trips: 85% ↔ 217).
            let pct = payload.first().copied().unwrap_or(0) as u32;
            out[1] = ((pct * 255 + 50) / 100).min(255) as u8;
        }
        RESHAPE_DPI => {
            // 0x2201 getSensorDpi payload = [sensorId, dpiHi, dpiLo, ...] (16-bit big-endian, MSB
            // first — lekensteyn x2201). `capability::dpi` reads args[1..5] as X_hi,X_lo,Y_hi,Y_lo.
            // HID++ reports ONE resolution (not per-axis), so mirror the single DPI onto X and Y.
            let hi = payload.get(1).copied().unwrap_or(0);
            let lo = payload.get(2).copied().unwrap_or(0);
            out[1] = hi;
            out[2] = lo;
            out[3] = hi;
            out[4] = lo;
        }
        _ => {
            // RESHAPE_NONE: the raw function payload, left-aligned (args[0..]).
            let n = payload.len().min(80);
            out[..n].copy_from_slice(&payload[..n]);
        }
    }
    out
}

/// The Logitech HID++ 2.0 family. A ZST, like [`crate::dialect::RazerDialect`].
pub struct HidppDialect;

impl Dialect for HidppDialect {
    fn id(&self) -> &'static str {
        "hidpp"
    }

    /// Claims a Logitech pipe that can actually COMPLETE a HID++ round-trip on this handle: a
    /// COHERENT bidirectional HID++ shape on a DIRECT-attached device. Both halves are required
    /// because [`HidppDialect::probe`]/[`HidppDialect::exec`] always `write_output` a request THEN
    /// `read_input` the reply (see the claiming-contract module doc) — a pipe advertising only ONE
    /// direction can never complete that flow, so it is NOT claimed (it lands on the `interested()`
    /// ledger instead):
    ///   * output side — a HID++ CLASS request report: short [`SHORT_LEN`] (7) or long [`LONG_LEN`] (20).
    ///   * input side — able to CARRY the reply: `input_len >= SHORT_LEN`. HID++ replies to a SHORT
    ///     request may arrive as a LONG report (libratbag hidpp-dissector: 0x10 short / 0x11 long), so
    ///     we require only ">= short", not an exact class length — a 7/20/64-byte input all qualify.
    ///
    /// Byte-length convention (stated so the next reader doesn't re-derive it): `output_len`/
    /// `input_len` are HIDP_CAPS `OutputReportByteLength`/`InputReportByteLength`, which INCLUDE the
    /// leading report-id byte. So `SHORT_LEN` 7 / `LONG_LEN` 20 here are the FULL on-wire lengths
    /// (report id + payload: 7 = id+6, 20 = id+19), directly comparable to HID++'s SHORT/LONG message
    /// lengths — no ±1 report-id adjustment is needed on either side.
    ///
    /// Receivers are excluded ([`RECEIVER_PIDS`]): they answer on HID++ 1.0 registers / pairing slots
    /// `1..=6`, not the corded [`DEVICE_INDEX`] `0xFF` this dialect addresses — see the DEVICE_INDEX note.
    fn claims(&self, info: &HidDeviceInfo) -> bool {
        let output_is_hidpp_class = info.output_len == SHORT_LEN || info.output_len == LONG_LEN as u16;
        let input_carries_reply = info.input_len >= SHORT_LEN;
        info.vid == HIDPP_VID
            && !is_receiver_pid(info.pid)
            && output_is_hidpp_class
            && input_carries_reply
    }

    /// HID++ control matching is the report-SHAPE claim, NOT the feature-report triple. `claims`
    /// carries the VID + 7/20-byte output/input test (the def's `feature_report_len` is meaningless
    /// here — HID++ never uses feature reports); the stored `usage_page`/`usage` pair then
    /// disambiguates WHICH claimable collection is the control pipe. A Logitech UNIFYING/BOLT
    /// receiver enumerates several HID++-shaped collections under one VID (the HID++ control
    /// collection plus keyboard/mouse consumer collections) — exactly why usage still participates:
    /// without it every one of those pipes would match the def.
    fn matches_control(&self, def: &crate::registry::DeviceDef, info: &HidDeviceInfo) -> bool {
        self.claims(info)
            && info.usage_page == def.control_interface.usage_page
            && info.usage == def.control_interface.usage
    }

    /// Widen to the whole Logitech VENDOR (like razer does for its audio sidecars): any 0x046D pipe
    /// is Logitech hardware even when its report shape isn't HID++, so a non-HID++ Logitech pipe
    /// lands on the unclaimed ledger instead of vanishing.
    fn interested(&self, info: &HidDeviceInfo) -> bool {
        info.vid == HIDPP_VID
    }

    /// Probe a live HID++ 2.0 pipe → a synthesized [`crate::synth::Synthesis`]. Read-only: ping
    /// IRoot.getProtocolVersion (require >= 2.0, same gate as [`probe_report`]), then enumerate the
    /// two features we know how to reshape (battery 0x1000, DPI 0x2201) and emit ONE getter command
    /// per feature that answered — with its reshape tag baked into `args[0]`. `None` when the pipe
    /// never returns a valid 2.0 ping (not a HID++ 2.0 pipe of ours).
    ///
    /// **Read-only by design — no setters.** A HID++ *set* (setSensorDpi, etc.) is a spec-implemented
    /// WRITE with no board to verify against; emitting one would put unproven bytes on the wire, the
    /// exact "no unproven bytes" rule this codebase graduated into types. Getters risk nothing (a read
    /// of a feature the device advertised), so adoption ships the reads and leaves writes for the
    /// wave that has hardware. The synthesized def is honest-but-UNVERIFIED and lands as
    /// `devices/auto/hidpp-<pid>.toml` (the dialect-keyed auto path); once a round-trip is confirmed
    /// the experimental marking lifts.
    fn probe(
        &self,
        t: &dyn Transport,
        ctx: &crate::synth::SynthCtx,
    ) -> Option<crate::synth::Synthesis> {
        use crate::registry::{CommandSpec, ControlInterface, DefOrigin, DeviceDef, Mode};
        use crate::synth::{Heuristic, Synthesis};
        use std::collections::BTreeMap;
        use std::time::Instant;

        // Ping IRoot.getProtocolVersion and TIME the round-trip. The ping is the one latency figure a
        // HID++ probe can honestly measure; unlike razer it does NOT feed `stream_wait_us` (there is
        // no streamed-lighting write to pace here — see `exec_fast`), so it only rides the emitted
        // file's provenance header. Require a valid 2.0 echo: a non-answering / 1.0 pipe is not ours.
        let ping = build_long(IROOT_FEATURE, IROOT_FN_GET_PROTOCOL, &[0x00, 0x00, PING_TOKEN]);
        let start = Instant::now();
        let reply = {
            // ONE request/reply conversation under the pipe's wire lock — per-pair, released before
            // the getFeature conversations below (each guards itself in `get_feature_index`).
            let wire = t.wire_lock();
            let _wire =
                wire.as_ref().map(|w| w.acquire());
            t.write_output(&ping).ok()?;
            recv_matching(t, IROOT_FEATURE, IROOT_FN_GET_PROTOCOL)?
        };
        let roundtrip_ms = start.elapsed().as_millis() as u64;
        let (major, echo) = (reply[4], reply[6]);
        if echo != PING_TOKEN || major < 0x02 {
            return None; // not a HID++ 2.0 device (or the echo didn't confirm)
        }

        // Enumerate the features we can reshape. `get_feature_index` maps "index 0" (device lacks it)
        // to `None`, so a mouse without a battery feature simply gets no battery command.
        let battery = get_feature_index(t, FEATURE_BATTERY);
        let dpi = get_feature_index(t, FEATURE_ADJUSTABLE_DPI);

        // One getter command per answered feature. `class` = the resolved feature INDEX (razer's
        // class slot), `id` = the function id, `size` = 0 (INERT: HID++ framing is fixed-shape, exec
        // ignores it), `args[0]` = the dialect-private reshape TAG (stripped before the wire, applied
        // to the reply). NO setters — writes without hardware to verify would break the no-unproven-
        // bytes rule (see the method doc); the reads are all this wave synthesizes.
        let mut commands: BTreeMap<String, CommandSpec> = BTreeMap::new();
        if let Some(idx) = battery {
            commands.insert(
                "battery_level".into(),
                CommandSpec { class: idx, id: FN_GET_BATTERY_STATUS, size: 0, args: vec![RESHAPE_BATTERY], transaction_id: None },
            );
        }
        if let Some(idx) = dpi {
            commands.insert(
                "dpi".into(),
                CommandSpec { class: idx, id: FN_GET_SENSOR_DPI, size: 0, args: vec![RESHAPE_DPI], transaction_id: None },
            );
        }
        // The evidence set the mint seam records: exactly the getters that answered (== the command
        // map keys, since we only inserted answered features). `from_probe` debug-asserts the subset.
        let answered: Vec<String> = commands.keys().cloned().collect();

        let def = DeviceDef {
            name: hidpp_name(&ctx.product, ctx.pid),
            codename: format!("auto-{:04x}", ctx.pid),
            dialect: "hidpp".into(),
            // Synthesized ⇒ Auto by construction (lands in devices/auto/, a reload re-stamps Auto).
            origin: DefOrigin::Auto,
            vendor_id: HIDPP_VID,
            // transaction_id is INERT for HID++ (fixed-shape framing; `exec`/`exec_fast` ignore it) —
            // 0 is the honest "unused" value, not an era guess. It rides `Heuristic` below only
            // because the wrapper slot is required, never because it means anything on the wire.
            transaction_id: 0,
            // No streamed-lighting path in this dialect ⇒ nothing to pace.
            stream_wait_us: 0,
            modes: vec![Mode { name: "default".into(), product_id: ctx.pid }],
            control_interface: ControlInterface {
                usage_page: ctx.usage_page,
                usage: ctx.usage,
                // Stored from the enumerated pipe for completeness, but HID++ MATCHING rides
                // `claims()` (VID + the 7/20-byte report shape), NOT the feature-report length —
                // HID++ uses output/input reports, so this length is informational here.
                feature_report_len: ctx.feature_len,
            },
            commands,
            // No lighting: HID++ lighting (feature 0x8070/…) is unspecified this wave.
            lighting: None,
            side_plates: None,
            // No push-report vocabulary probed this wave — an auto HID++ def carries none.
            events: None,
        };

        // Mint through the cross-dialect seam: tx/dims/stream_wait are all Heuristic/None (HID++ has
        // no era inference and no lighting), serial is empty (no serial getter probed), and the ping
        // round-trip is the measured figure. `from_probe` mints the Proven from `answered`.
        Some(Synthesis::from_probe(
            def,
            answered,
            Heuristic(0),
            None,
            Heuristic(0),
            String::new(),
            roundtrip_ms,
        ))
    }

    /// One framed request → 80-byte reshaped reply. Spec interpretation: `spec_class` = feature
    /// INDEX, `spec_id` = function id, `args[0]` = reshape tag, `args[1..]` = wire fn params. The
    /// razer `transaction_id`/`size` fields are INERT here (HID++ framing is fixed-shape) — ignored.
    fn exec(
        &self,
        t: &dyn Transport,
        transaction_id: u8,
        spec_class: u8,
        spec_id: u8,
        size: u8,
        args: &[u8],
    ) -> Result<[u8; 80]> {
        let _ = (transaction_id, size); // razer vocabulary; no meaning in HID++

        // Hold the pipe's wire lock for the WHOLE conversation (write → read/drain): pair-atomicity
        // is the unit; between conversations other actors may interleave freely.
        let wire = t.wire_lock();
        let _wire = wire.as_ref().map(|w| w.acquire());

        let feature_idx = spec_class;
        let fn_id = spec_id;
        let reshape_tag = args.first().copied().unwrap_or(RESHAPE_NONE);
        let params = args.get(1..).unwrap_or(&[]); // strip the dialect-private tag byte

        let req = build_long(feature_idx, fn_id, params);
        t.write_output(&req)?;

        // Drain input reports (~10 reads / ~1s) for our reply or an error report.
        for _ in 0..10 {
            let mut buf = [0u8; LONG_LEN];
            let n = match t.read_input(&mut buf, 100) {
                Ok(n) => n,
                Err(_) => continue, // window empty; keep waiting
            };
            let got = &buf[..n.min(LONG_LEN)];
            if let Some(code) = error_code(got) {
                bail!(
                    "HID++ error {code:#04x} ({}) for feature {feature_idx:#06x} fn {fn_id:#x}",
                    err_name(code)
                );
            }
            if matches_reply(got, feature_idx, fn_id) {
                return Ok(reshape(reshape_tag, &buf[4..]));
            }
        }
        bail!("HID++ timed out waiting for reply to feature {feature_idx:#06x} fn {fn_id:#x}")
    }

    /// Fire-and-forget streamed write. HID++ has no streamed-lighting path in wave 3 (probe
    /// synthesizes none), so this is minimal-but-honest: build the same long report `exec` would and
    /// write it once, no reply drain. `args[0]` is the reshape tag (setters use `RESHAPE_NONE`),
    /// stripped for wire-parameter parity with `exec`. `transaction_id`/`size` inert.
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
        let _ = (transaction_id, size);
        let params = args.get(1..).unwrap_or(&[]);
        let req = build_long(class, id, params);
        let _ = t.write_output(&req);
        if stream_wait_us > 0 {
            std::thread::sleep(std::time::Duration::from_micros(stream_wait_us));
        }
    }

    // NB: `release_custody` is INTENTIONALLY NOT overridden — the trait's default no-op is the
    // correct release for HID++. Custody ("driver mode is a lease; firmware owns the rest state") is
    // a RAZER-FAMILY contract; a HID++ device is never in our custody (no device-mode concept), so
    // there is nothing to hand back. Framing razer's 0x00/0x04 mode write here would put a validly-
    // shaped HID++ message with garbage meaning on the wire — exactly what the per-family hook exists
    // to prevent.
}

/// What [`probe_report`] learned about a live HID++ pipe — the read-only diagnostic that stands in
/// for full adoption this wave. Describes what WOULD be synthesized (protocol version + the feature
/// indices found) without minting a `Synthesis`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HidppProbe {
    pub protocol_major: u8,
    pub protocol_minor: u8,
    /// Resolved feature INDEX for 0x1000 BatteryLevelStatus, when the device reports it.
    pub battery_feature: Option<u8>,
    /// Resolved feature INDEX for 0x2201 AdjustableDPI, when the device reports it.
    pub dpi_feature: Option<u8>,
}

/// Diagnostic probe of a live HID++ pipe: ping IRoot.getProtocolVersion (require >= 2.0), then
/// IRoot.getFeature() for battery (0x1000) and DPI (0x2201). Read-only — getters only, never a
/// blind write into an unknown framing. `None` if the pipe never answered a valid 2.0 ping (so it
/// is not a HID++ 2.0 pipe of ours). [`HidppDialect::probe`] now builds on this exact wire path to
/// synthesize a real def; this stays as the standalone READ-ONLY diagnostic (protocol + feature
/// indices, no def minted) that someone-with-hardware runs to confirm the spec against their device.
pub fn probe_report(t: &dyn Transport) -> Option<HidppProbe> {
    // getProtocolVersion: params [0x00, 0x00, pingData] — the two leading zeros + echoed ping are
    // the documented HID++ 1.0/2.0 disambiguation trick. Reply payload = [major, minor, pingEcho].
    let ping = build_long(IROOT_FEATURE, IROOT_FN_GET_PROTOCOL, &[0x00, 0x00, PING_TOKEN]);
    let reply = {
        // ONE request/reply conversation under the pipe's wire lock — per-pair, released before the
        // getFeature conversations below (each takes its own guard inside `get_feature_index`).
        let wire = t.wire_lock();
        let _wire = wire.as_ref().map(|w| w.acquire());
        t.write_output(&ping).ok()?;
        recv_matching(t, IROOT_FEATURE, IROOT_FN_GET_PROTOCOL)?
    };
    let (major, minor, echo) = (reply[4], reply[5], reply[6]);
    if echo != PING_TOKEN || major < 0x02 {
        return None; // not a HID++ 2.0 device (or the echo didn't confirm)
    }
    Some(HidppProbe {
        protocol_major: major,
        protocol_minor: minor,
        battery_feature: get_feature_index(t, FEATURE_BATTERY),
        dpi_feature: get_feature_index(t, FEATURE_ADJUSTABLE_DPI),
    })
}

/// IRoot.getFeature(featureId) → feature INDEX, or `None` when the device lacks it (index 0 is
/// reserved for IRoot itself, so 0 means "not present"). featureId is sent big-endian (MSB first).
fn get_feature_index(t: &dyn Transport, feature_id: u16) -> Option<u8> {
    let params = [(feature_id >> 8) as u8, feature_id as u8];
    let req = build_long(IROOT_FEATURE, IROOT_FN_GET_FEATURE, &params);

    // ONE request/reply conversation under the pipe's wire lock (per-pair, like every probe site).
    let wire = t.wire_lock();
    let _wire = wire.as_ref().map(|w| w.acquire());

    t.write_output(&req).ok()?;
    let reply = recv_matching(t, IROOT_FEATURE, IROOT_FN_GET_FEATURE)?;
    let idx = reply[4]; // payload[0] = featureIndex
    if idx == 0 {
        None
    } else {
        Some(idx)
    }
}

/// The synthesized def's honest name, from the USB product string with a pid fallback. synth's
/// `comment_safe`/`toml_escape` are private to that module, so replicate the ONE rule the emitted-
/// file round-trip needs: collapse ASCII control bytes to spaces (a USB product string is
/// attacker-adjacent junk; a raw control byte in `name = "…"` would corrupt the TOML). Quotes and
/// backslashes are left for synth's `toml_escape` to escape at emit time (they round-trip losslessly),
/// so the in-memory name here equals what a reload of the emitted file yields.
fn hidpp_name(product: &str, pid: u16) -> String {
    let trimmed = product.trim();
    if trimmed.is_empty() {
        format!("Logitech device {pid:04x}")
    } else {
        trimmed
            .chars()
            .map(|c| if (c as u32) < 0x20 || c == '\u{7F}' { ' ' } else { c })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::DevicePath;
    use std::collections::VecDeque;
    use std::sync::Mutex;

    /// A scripted HID++ mock: records every OUTPUT report written and replays canned INPUT frames
    /// in FIFO order. Implements ONLY the output/input surface (the feature-report methods bail, as
    /// a real HID++ collection would). This is the whole point of the transport extension.
    struct MockHidpp {
        writes: Mutex<Vec<[u8; LONG_LEN]>>,
        inbox: Mutex<VecDeque<Vec<u8>>>,
    }

    impl MockHidpp {
        fn with_replies(frames: Vec<Vec<u8>>) -> Self {
            MockHidpp {
                writes: Mutex::new(Vec::new()),
                inbox: Mutex::new(frames.into_iter().collect()),
            }
        }
        fn last_write(&self) -> [u8; LONG_LEN] {
            *self.writes.lock().unwrap().last().expect("a request was written")
        }
    }

    impl Transport for MockHidpp {
        fn set_feature(&self, _buf: &[u8]) -> Result<()> {
            bail!("hidpp mock carries no feature reports")
        }
        fn get_feature(&self, _buf: &mut [u8]) -> Result<()> {
            bail!("hidpp mock carries no feature reports")
        }
        fn write_output(&self, buf: &[u8]) -> Result<()> {
            let mut b = [0u8; LONG_LEN];
            let n = buf.len().min(LONG_LEN);
            b[..n].copy_from_slice(&buf[..n]);
            self.writes.lock().unwrap().push(b);
            Ok(())
        }
        fn read_input(&self, buf: &mut [u8], _timeout_ms: u32) -> Result<usize> {
            match self.inbox.lock().unwrap().pop_front() {
                Some(frame) => {
                    let n = frame.len().min(buf.len());
                    buf[..n].copy_from_slice(&frame[..n]);
                    Ok(n)
                }
                None => bail!("mock inbox empty (no more scripted replies)"),
            }
        }
    }

    /// Pad a leading byte sequence out to a full 20-byte long frame.
    fn frame(prefix: &[u8]) -> Vec<u8> {
        let mut v = vec![0u8; LONG_LEN];
        v[..prefix.len()].copy_from_slice(prefix);
        v
    }

    fn info(vid: u16, output_len: u16, input_len: u16) -> HidDeviceInfo {
        HidDeviceInfo {
            vid,
            pid: 0,
            usage_page: 0,
            usage: 0,
            feature_len: 0,
            input_len,
            output_len,
            path: DevicePath::from_str_for_tests("x"),
            product: String::new(),
        }
    }

    #[test]
    fn getprotocolversion_request_matches_golden() {
        // The exact 20-byte getProtocolVersion request: [0x11, 0xFF, 0x00, 0x1A, 0x00, 0x00, ping].
        // 0x1A = (function 1 << 4) | softwareId 0x0A. Confirms build_long framing byte-for-byte.
        let mut want = [0u8; LONG_LEN];
        want[0] = 0x11; // long report id
        want[1] = 0xFF; // device index (receiver-direct default)
        want[2] = 0x00; // IRoot feature index
        want[3] = 0x1A; // (getProtocolVersion=1 << 4) | swId 0x0A
        want[4] = 0x00; // ping param byte 0
        want[5] = 0x00; // ping param byte 1
        want[6] = PING_TOKEN; // echoed ping token
        let built = build_long(IROOT_FEATURE, IROOT_FN_GET_PROTOCOL, &[0x00, 0x00, PING_TOKEN]);
        assert_eq!(built, want, "getProtocolVersion request must be byte-identical to the golden");
    }

    #[test]
    fn probe_report_round_trips_protocol_and_features() {
        // Scripted: ping reply (major 0x04 = HID++ 2.0, echo 0x5A), then getFeature replies giving
        // battery feature index 0x06 and DPI feature index 0x07.
        let mock = MockHidpp::with_replies(vec![
            frame(&[0x11, 0xFF, 0x00, 0x1A, 0x04, 0x02, PING_TOKEN]), // getProtocolVersion reply
            frame(&[0x11, 0xFF, 0x00, 0x0A, 0x06]), // getFeature(0x1000) → index 6
            frame(&[0x11, 0xFF, 0x00, 0x0A, 0x07]), // getFeature(0x2201) → index 7
        ]);
        let report = probe_report(&mock).expect("valid 2.0 ping");
        assert_eq!(report.protocol_major, 0x04);
        assert_eq!(report.protocol_minor, 0x02);
        assert_eq!(report.battery_feature, Some(0x06));
        assert_eq!(report.dpi_feature, Some(0x07));
        // The FIRST thing written must be the exact ping golden.
        let first = mock.writes.lock().unwrap()[0];
        assert_eq!(first[..7], [0x11, 0xFF, 0x00, 0x1A, 0x00, 0x00, PING_TOKEN]);
    }

    #[test]
    fn error_report_maps_to_error_with_code() {
        // exec for feature 0x06 fn 0; the device answers with a 0x8F error, code 0x02 (InvalidArgument).
        let mock = MockHidpp::with_replies(vec![frame(&[
            0x10, 0xFF, ERR_HIDPP10, 0x06, func_swid(0x00), 0x02,
        ])]);
        let err = HidppDialect
            .exec(&mock, 0x00, 0x06, 0x00, 0x00, &[RESHAPE_NONE])
            .expect_err("an error report must map to Err");
        let msg = format!("{err}");
        assert!(msg.contains("InvalidArgument"), "error names the code: {msg}");
    }

    #[test]
    fn battery_reply_reshapes_percentage_into_args1() {
        // Feature 0x06 fn 0 (battery), tag RESHAPE_BATTERY. Device reports 85% at payload[0].
        // capability::battery_percent reads args[1]; 85% must land as round(85*255/100) = 217.
        let mock = MockHidpp::with_replies(vec![frame(&[
            0x11,
            0xFF,
            0x06,
            func_swid(0x00),
            85, // batteryDischargeLevel (percentage) at payload[0]
            80, // next level
            0x00, // status
        ])]);
        let out = HidppDialect
            .exec(&mock, 0x00, 0x06, 0x00, 0x00, &[RESHAPE_BATTERY])
            .expect("battery reply");
        assert_eq!(out[1], 217, "85% scaled into the 0..255 space at args[1]");
        assert_eq!(out[0], 0, "args[0] stays clear (the semantic decoder ignores it)");
        // And the request stripped the tag: params are empty, feature/fn framed correctly.
        assert_eq!(mock.last_write()[..4], [0x11, 0xFF, 0x06, func_swid(0x00)]);
    }

    #[test]
    fn dpi_reply_mirrors_single_resolution_to_xy() {
        // getSensorDpi payload [sensorId, dpiHi, dpiLo] = [0, 0x03, 0x20] → 800 DPI, big-endian.
        let mock = MockHidpp::with_replies(vec![frame(&[
            0x11,
            0xFF,
            0x07,
            func_swid(0x02),
            0x00, // sensorId
            0x03, // dpiHi
            0x20, // dpiLo  (0x0320 = 800)
        ])]);
        let out = HidppDialect
            .exec(&mock, 0x00, 0x07, 0x02, 0x00, &[RESHAPE_DPI, 0x00])
            .expect("dpi reply");
        // capability::dpi reads args[1..5] big-endian X_hi,X_lo,Y_hi,Y_lo — X and Y both 800.
        assert_eq!([out[1], out[2], out[3], out[4]], [0x03, 0x20, 0x03, 0x20]);
    }

    fn info_pid(vid: u16, pid: u16, output_len: u16, input_len: u16) -> HidDeviceInfo {
        HidDeviceInfo { pid, ..info(vid, output_len, input_len) }
    }

    #[test]
    fn claims_matrix() {
        // A claimable pipe needs BOTH a HID++ CLASS output report AND an input able to carry the reply.
        assert!(HidppDialect.claims(&info(HIDPP_VID, 20, 20)), "20-byte out + 20-byte in = HID++ long, bidirectional");
        assert!(HidppDialect.claims(&info(HIDPP_VID, 7, 20)), "7-byte out + long in = short request, long reply");
        assert!(HidppDialect.claims(&info(HIDPP_VID, 7, 7)), "7-byte out + 7-byte in = HID++ short, both directions");
        assert!(HidppDialect.claims(&info(HIDPP_VID, 20, 64)), "input >= short carries the reply (a 64-byte input qualifies)");
        // One-direction collections can never complete write_output → read_input:
        assert!(!HidppDialect.claims(&info(HIDPP_VID, 20, 0)), "output-only (no input report) can't carry a reply");
        assert!(!HidppDialect.claims(&info(HIDPP_VID, 7, 0)), "output-only short pipe can't carry a reply");
        assert!(!HidppDialect.claims(&info(HIDPP_VID, 0, 20)), "input-only (no output report) can't send a request");
        // Wrong report shape / wrong vendor:
        assert!(!HidppDialect.claims(&info(HIDPP_VID, 64, 64)), "Logitech + 64-byte report ≠ HID++");
        assert!(!HidppDialect.claims(&info(0x1532, 91, 0)), "Razer VID ≠ HID++");
    }

    #[test]
    fn claims_excludes_receivers_but_stays_interested() {
        // A Unifying receiver (0xC52B) with a picture-perfect bidirectional HID++ shape is still NOT
        // claimable: it answers on HID++ 1.0 registers / paired slots, not the 0xFF 2.0 endpoint the
        // probe pings. It stays INTERESTED so it surfaces as unclaimed inventory, never as a claimed-
        // but-unresponsive device row.
        let recv = info_pid(HIDPP_VID, 0xC52B, LONG_LEN as u16, LONG_LEN as u16);
        assert!(!HidppDialect.claims(&recv), "a receiver pid is never claimed, even with a perfect HID++ shape");
        assert!(HidppDialect.interested(&recv), "but the receiver still surfaces on the unclaimed ledger");
        // Bolt (0xC548) and a Lightspeed (0xC545) receiver likewise excluded.
        assert!(!HidppDialect.claims(&info_pid(HIDPP_VID, 0xC548, LONG_LEN as u16, LONG_LEN as u16)), "Bolt receiver not claimed");
        assert!(!HidppDialect.claims(&info_pid(HIDPP_VID, 0xC545, LONG_LEN as u16, LONG_LEN as u16)), "Lightspeed receiver not claimed");
        // The exclusion is pid-scoped: a DIRECT-attached mouse pid with the same shape IS claimed.
        assert!(HidppDialect.claims(&info_pid(HIDPP_VID, 0xC088, LONG_LEN as u16, LONG_LEN as u16)), "a direct-attached mouse pid with a HID++ shape is claimed");
    }

    /// A minimal hidpp-tagged DeviceDef with a chosen control usage pair (feature_report_len is
    /// deliberately 0 — meaningless for HID++). vendor_id 1133 = 0x046D.
    fn hidpp_def(usage_page: u16, usage: u16) -> crate::registry::DeviceDef {
        let text = format!(
            "name = \"L\"\ncodename = \"l\"\ndialect = \"hidpp\"\nvendor_id = 1133\ntransaction_id = 0\n\
             [[modes]]\nname = \"default\"\nproduct_id = 1\n\
             [control_interface]\nusage_page = {usage_page}\nusage = {usage}\nfeature_report_len = 0\n\
             [commands]\n"
        );
        toml::from_str(&text).expect("minimal hidpp test def parses")
    }

    #[test]
    fn matches_control_rides_shape_plus_usage_not_feature_len() {
        // HID++ control matching = claims() (VID + 7/20-byte report shape) AND the stored usage pair;
        // the feature_report_len is meaningless. The usage pair is what disambiguates a receiver's
        // several HID++-shaped collections — right shape, wrong usage is a SIBLING pipe, not control.
        let def = hidpp_def(0x0001, 0x0002);
        let pipe = |up: u16, us: u16, out: u16, inp: u16| HidDeviceInfo {
            vid: HIDPP_VID,
            pid: 0,
            usage_page: up,
            usage: us,
            feature_len: 0,
            input_len: inp,
            output_len: out,
            path: DevicePath::from_str_for_tests("x"),
            product: String::new(),
        };
        assert!(
            def.matches_control(&pipe(0x0001, 0x0002, LONG_LEN as u16, LONG_LEN as u16)),
            "Logitech + bidirectional 20-byte shape + matching usage = the control pipe"
        );
        assert!(
            !def.matches_control(&pipe(0x000c, 0x0001, LONG_LEN as u16, LONG_LEN as u16)),
            "right report SHAPE but a different usage = a sibling collection, not control"
        );
        assert!(
            !def.matches_control(&pipe(0x0001, 0x0002, 64, 64)),
            "matching usage but a non-HID++ report shape = not claimed at all"
        );
    }

    #[test]
    fn interested_widens_to_any_logitech() {
        assert!(HidppDialect.interested(&info(HIDPP_VID, 64, 64)), "any Logitech pipe → interested");
        assert!(HidppDialect.interested(&info(HIDPP_VID, 0, 0)));
        assert!(!HidppDialect.interested(&info(0x1532, 91, 0)), "non-Logitech vendor → not ours");
    }

    /// A HID++ ctx shaped like a live Logitech mouse pipe.
    fn hidpp_ctx(product: &str) -> crate::synth::SynthCtx {
        crate::synth::SynthCtx {
            vid: HIDPP_VID,
            pid: 0xC088,
            usage_page: 0x0001,
            usage: 0x0002,
            feature_len: 0, // HID++ has no feature reports; matching rides claims()
            product: String::from(product),
        }
    }

    #[test]
    fn probe_synthesizes_a_hidpp_def_and_battery_round_trips() {
        use crate::registry::DefOrigin;
        // Scripted: 2.0 ping, battery feature idx 6, DPI feature idx 7, then a battery reply for the
        // exec below (the mock inbox is FIFO — probe drains the first three, exec drains the fourth).
        let mock = MockHidpp::with_replies(vec![
            frame(&[0x11, 0xFF, 0x00, 0x1A, 0x04, 0x02, PING_TOKEN]), // getProtocolVersion → 4.2
            frame(&[0x11, 0xFF, 0x00, 0x0A, 0x06]),                   // getFeature(0x1000) → idx 6
            frame(&[0x11, 0xFF, 0x00, 0x0A, 0x07]),                   // getFeature(0x2201) → idx 7
            frame(&[0x11, 0xFF, 0x06, func_swid(FN_GET_BATTERY_STATUS), 85]), // battery: 85%
        ]);
        let s = HidppDialect
            .probe(&mock, &hidpp_ctx("Logitech G Pro"))
            .expect("a valid 2.0 pipe synthesizes Some");
        let d = &s.def;
        assert_eq!(d.dialect, "hidpp", "def tagged with the synthesizing dialect");
        assert_eq!(d.origin, DefOrigin::Auto, "synthesized ⇒ Auto");
        assert_eq!(d.name, "Logitech G Pro");
        // The battery command carries the resolved feature index + the reshape tag at args[0].
        let bat = d.command("battery_level").expect("battery command synthesized");
        assert_eq!(bat.class, 0x06, "class = resolved battery feature index");
        assert_eq!(bat.id, FN_GET_BATTERY_STATUS);
        assert_eq!(bat.args.first().copied(), Some(RESHAPE_BATTERY), "reshape tag 0x01 at args[0]");
        // DPI likewise; and NO setters (read-only until hardware verification exists).
        assert_eq!(d.command("dpi").map(|c| c.class), Some(0x07), "class = resolved DPI feature index");
        assert!(d.command("set_dpi").is_none(), "no DPI setter synthesized");
        assert!(d.command("set_brightness").is_none(), "no setters at all");
        assert!(d.lighting.is_none(), "no lighting block");
        // Battery round-trip THROUGH the synthesized def: exec the spec, then decode args[1] with
        // capability::battery_percent's exact math ((raw*100+127)/255) — it must recover the 85%.
        let out = HidppDialect
            .exec(&mock, d.transaction_id, bat.class, bat.id, bat.size, &bat.args)
            .expect("battery reply reshapes");
        let pct = (out[1] as u32 * 100 + 127) / 255;
        assert_eq!(pct, 85, "reshaped args[1] decodes back to the reported 85%");
    }

    #[test]
    fn probe_returns_none_when_the_pipe_never_answers() {
        // Empty inbox: read_input bails every drain window, so the ping never resolves → not ours.
        let mock = MockHidpp::with_replies(vec![]);
        assert!(
            HidppDialect.probe(&mock, &hidpp_ctx("")).is_none(),
            "no ping reply → not a HID++ 2.0 pipe → None"
        );
    }

    #[test]
    fn emitted_hidpp_toml_round_trips_with_the_dialect_tag() {
        use crate::registry::DeviceDef;
        // Probe (no exec this time, so three replies suffice), emit, and parse the file back.
        let mock = MockHidpp::with_replies(vec![
            frame(&[0x11, 0xFF, 0x00, 0x1A, 0x04, 0x02, PING_TOKEN]),
            frame(&[0x11, 0xFF, 0x00, 0x0A, 0x06]),
            frame(&[0x11, 0xFF, 0x00, 0x0A, 0x07]),
        ]);
        let s = HidppDialect.probe(&mock, &hidpp_ctx("Logitech G Pro")).unwrap();
        let text = crate::synth::emit_toml(&s);
        let mut parsed: DeviceDef = toml::from_str(&text)
            .unwrap_or_else(|e| panic!("emitted hidpp TOML must parse: {e}\n---\n{text}"));
        // `origin` is a serde-skipped LOAD fact (a bare parse defaults Builtin) — reconcile it like
        // synth's own round-trip tests before the equality; the FILE content is what's lossless.
        parsed.origin = s.def.origin.clone();
        assert_eq!(parsed.dialect, "hidpp", "the non-razer dialect tag survives the round-trip");
        assert_eq!(parsed, s.def, "emit → parse must be lossless for a hidpp def");
    }
}
