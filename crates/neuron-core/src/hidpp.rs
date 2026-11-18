// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

//! Logitech HID++ 2.0 — dialect #2 (DIALECT-RND wave 3). **EXPERIMENTAL, spec-implemented, ZERO
//! hardware verification.** Every byte layout here is transcribed from the libratbag / Solaar /
//! Logitech `cpg-docs` documentation (URLs in the frame/reshape comments below), NOT observed on a
//! wire — there is no Logitech device on the development desk. The dialect CLAIMS its pipes, is
//! INTERESTED at vendor granularity, and now ADOPTS: [`HidppDialect::probe`] is REAL — it pings
//! `IRoot`, enumerates the battery/DPI features, and synthesizes a [`crate::synth::Synthesis`] via the
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
//! softwareId so a client can tell its own replies apart. This implementation frames requests as
//! LONG reports and uses a fixed [`SW_ID`] nibble. Pipes that only advertise SHORT output reports
//! are not claimed until short-request framing is implemented. HID++ 2.0 is big-endian for
//! multi-byte values.
//!
//! ## The semantic reply contract (the load-bearing design point)
//! Everything above the dialect line is dialect-BLIND: `capability::battery_percent` reads
//! `args[1]` as a 0..255 raw level and scales it `*100/255`; `capability::dpi` reads `args[1..5]`
//! as big-endian `X_hi,X_lo,Y_hi,Y_lo`. Those decoders must keep working unchanged for a HID++ mouse.
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
/// The receiver itself at [`DEVICE_INDEX`] `0xFF` speaks HID++ 1.0 registers; the devices paired to it
/// answer the 2.0 feature protocol on pairing slots `1..=6` ([`PAIRING_SLOTS`]), so a receiver is
/// adopted through the first slot whose device answers. List transcribed from Solaar `lib/logitech_receiver/base_usb.py` (the canonical VID-0x046D
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

/// Is `pid` one of the [`RECEIVER_PIDS`]?
fn is_receiver_pid(pid: u16) -> bool {
    RECEIVER_PIDS.contains(&pid)
}

const REPORT_ID_SHORT: u8 = 0x10; // 7-byte short report
const REPORT_ID_LONG: u8 = 0x11; // 20-byte long report
const SHORT_LEN: u16 = 7;
const LONG_LEN: usize = 20;

/// Device index of a direct-attached (corded or Bluetooth) HID++ 2.0 device (cpg-docs).
const DEVICE_INDEX: u8 = 0xFF;

/// The pairing slots a receiver's devices answer on: a receiver holds at most six (Solaar
/// `receiver.py` L178-L187 @e7304c4c). It answers an empty or unreachable slot at once with a HID++
/// 1.0 error.
const PAIRING_SLOTS: std::ops::RangeInclusive<u8> = 1..=6;

/// The device index a def's `transaction_id` names: 0 (no razer meaning here) is the direct device,
/// 1..=6 a receiver's pairing slot.
fn address_of(transaction_id: u8) -> u8 {
    if transaction_id == 0 { DEVICE_INDEX } else { transaction_id }
}

/// Our fixed softwareId nibble (byte 3 low nibble); a reply echoes it so we can match our own.
/// Other clients sharing the device claim fixed ids (Solaar `base.py` @e7304c4c: 0x07 OpenRGB,
/// 0x0A LGSTrayEx, 0x0B Solaar, 0x0D G HUB, 0x0F firmware); 0x0C collides with none and keeps the
/// high bit set so notifications (swId 0) stay distinct.
const SW_ID: u8 = 0x0C;

/// Reply wait, by time: a busy device interleaves notifications ahead of our reply, so frames that
/// aren't ours never shorten the wait (Solaar waits by deadline too). Silence ends it sooner.
const REPLY_DEADLINE: std::time::Duration = std::time::Duration::from_secs(2);
const READ_WINDOW_MS: u32 = 100;
const MAX_SILENT_WINDOWS: u32 = 10;

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
/// 0x2202 getSensorDpiParameters: X and Y read separately.
pub const RESHAPE_DPI_XY: u8 = 0x03;

/// Feature ids the probe looks for (documented HID++ 2.0 feature numbers).
pub const FEATURE_BATTERY: u16 = 0x1000; // BatteryLevelStatus
pub const FEATURE_ADJUSTABLE_DPI: u16 = 0x2201; // AdjustableDPI
/// UnifiedBattery, the battery feature on most Logitech devices since 0x1000; asked when 0x1000 is
/// absent (Solaar `hidpp20.py` L1901-L1904 @e7304c4c).
pub const FEATURE_UNIFIED_BATTERY: u16 = 0x1004;
/// ExtendedAdjustableDPI, the per-axis DPI feature; asked when 0x2201 is absent (Solaar
/// `settings_templates.py` L1088-L1140 @e7304c4c).
pub const FEATURE_EXTENDED_DPI: u16 = 0x2202;
/// DeviceName.
pub const FEATURE_DEVICE_NAME: u16 = 0x0005;
const FN_NAME_COUNT: u8 = 0x00;
const FN_NAME_FRAGMENT: u8 = 0x01;

/// Function ids WITHIN those features, baked into the synthesized command specs as `spec_id` (the
/// razer `id` slot). Function 0 of 0x1000 is getBatteryLevelStatus; getSensorDpi is the read on
/// 0x2201 — matched to the [`reshape`]/`exec` golden tests' `func_swid(0x02)` DPI convention.
const FN_GET_BATTERY_STATUS: u8 = 0x00; // 0x1000 getBatteryLevelStatus → discharge % at payload[0]
const FN_GET_SENSOR_DPI: u8 = 0x02; // 0x2201 getSensorDpi → single big-endian resolution
const FN_UNIFIED_GET_STATUS: u8 = 0x01; // 0x1004 getStatus → [state of charge %, level, status, ..]
const FN_EXTENDED_GET_SENSOR_DPI: u8 = 0x05; // 0x2202 getSensorDpiParameters(sensor) → [sensor, X, X default, Y, Y default, ..]

/// Build a 20-byte LONG request. `params` past the 16-byte body is dropped (a long report holds 16).
fn build_long(index: u8, feature_idx: u8, fn_id: u8, params: &[u8]) -> [u8; LONG_LEN] {
    let mut r = [0u8; LONG_LEN];
    r[0] = REPORT_ID_LONG;
    r[1] = index;
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

/// Human name for an error code. HID++ 1.0 (marker 0x8F) and 2.0 (0xFF) number their errors
/// differently: 1.0 per Solaar `hidpp10_constants.py` @e7304c4c, 2.0 per cpg-docs.
fn err_name(marker: u8, code: u8) -> &'static str {
    if marker == ERR_HIDPP10 {
        return match code {
            0x01 => "InvalidSubId",
            0x02 => "InvalidAddress",
            0x03 => "InvalidValue",
            0x04 => "ConnectionFailed",
            0x05 => "TooManyDevices",
            0x06 => "AlreadyExists",
            0x07 => "Busy",
            0x08 => "UnknownDevice",
            0x09 => "ResourceError",
            0x0A => "RequestUnavailable",
            0x0B => "UnsupportedParameterValue",
            0x0C => "WrongPinCode",
            _ => "unspecified",
        };
    }
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

/// Standard HID++ report IDs carry fixed wire lengths; the caller's buffer capacity is not the read length.
fn valid_report_shape(buf: &[u8]) -> bool {
    match buf.first().copied() {
        Some(REPORT_ID_SHORT) => buf.len() == SHORT_LEN as usize,
        Some(REPORT_ID_LONG) => buf.len() == LONG_LEN,
        _ => false,
    }
}

fn logical_report<'a>(t: &dyn Transport, buf: &'a [u8]) -> Option<&'a [u8]> {
    if valid_report_shape(buf) {
        Some(buf)
    } else if t.padded_input_reports() && buf.len() == LONG_LEN && buf.first() == Some(&REPORT_ID_SHORT) {
        // Windows ReadFile returns the collection maximum, even for a short report.
        Some(&buf[..SHORT_LEN as usize])
    } else {
        None
    }
}

/// A Bluetooth-direct device may answer on index 0x00 instead of the 0xFF it was addressed on
/// (Solaar `base.py` @e7304c4c accepts `devnumber ^ 0xFF`).
fn is_our_device(want: u8, got: u8) -> bool {
    got == want || (want == DEVICE_INDEX && got == DEVICE_INDEX ^ 0xFF)
}

/// The `(marker, code)` of an error report for OUR request to `(feature_idx, fn_id)`. Both 1.0
/// (0x8F) and 2.0 (0xFF) errors echo the request's feature index and function|swId at bytes 3-4;
/// an error for another client's request (G HUB, Solaar) on the same device is not ours.
fn error_for(buf: &[u8], index: u8, feature_idx: u8, fn_id: u8) -> Option<(u8, u8)> {
    (valid_report_shape(buf)
        && is_our_device(index, buf[1])
        && (buf[2] == ERR_HIDPP10 || buf[2] == ERR_HIDPP20)
        && buf[3] == feature_idx
        && buf[4] == func_swid(fn_id))
    .then(|| (buf[2], buf[5]))
}

/// Does `buf` echo a normal reply to `(feature_idx, fn_id)` for us? (report id short/long, our
/// device index, echoed feature index, echoed function|softwareId, and the complete report length.)
fn matches_reply(buf: &[u8], index: u8, feature_idx: u8, fn_id: u8) -> bool {
    valid_report_shape(buf)
        && is_our_device(index, buf[1])
        && buf[2] == feature_idx
        && buf[3] == func_swid(fn_id)
}

enum Answer {
    Reply([u8; LONG_LEN]),
    Error { marker: u8, code: u8 },
    Silent,
}

/// Wait for the answer to `(feature_idx, fn_id)`, skipping notifications and other clients'
/// traffic until [`REPLY_DEADLINE`], or until the device has been silent for
/// [`MAX_SILENT_WINDOWS`] read windows.
fn await_reply(t: &dyn Transport, index: u8, feature_idx: u8, fn_id: u8) -> Answer {
    let deadline = std::time::Instant::now() + REPLY_DEADLINE;
    let mut silent = 0;
    while silent < MAX_SILENT_WINDOWS {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        if left.is_zero() {
            break;
        }
        let window = (left.as_millis() as u32).clamp(1, READ_WINDOW_MS);
        let mut buf = [0u8; LONG_LEN];
        let n = match t.read_input(&mut buf, window) {
            Ok(n) => n,
            Err(_) => {
                silent += 1;
                continue;
            }
        };
        if n > buf.len() {
            continue;
        }
        let Some(got) = logical_report(t, &buf[..n]) else { continue; };
        if let Some((marker, code)) = error_for(got, index, feature_idx, fn_id) {
            return Answer::Error { marker, code };
        }
        if matches_reply(got, index, feature_idx, fn_id) {
            let mut reply = [0u8; LONG_LEN];
            reply[..got.len()].copy_from_slice(got);
            return Answer::Reply(reply);
        }
    }
    Answer::Silent
}

/// The normal reply to `(feature_idx, fn_id)`, or `None` for an error or silence; the diagnostic
/// callers treat both as "feature absent / didn't answer".
fn recv_matching(t: &dyn Transport, index: u8, feature_idx: u8, fn_id: u8) -> Option<[u8; LONG_LEN]> {
    match await_reply(t, index, feature_idx, fn_id) {
        Answer::Reply(r) => Some(r),
        Answer::Error { .. } | Answer::Silent => None,
    }
}

/// Logitech webcams answer the HID++ check badly enough to lock up (Solaar `hidapi_impl.py`
/// @e7304c4c skips this PID range for that reason).
fn is_webcam_pid(pid: u16) -> bool {
    (0x0800..=0x09FF).contains(&pid)
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
            let pct = u32::from(payload.first().copied().unwrap_or(0));
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
        RESHAPE_DPI_XY => {
            // [sensor, X hi, X lo, X default hi, lo, Y hi, Y lo, ...]; a single-axis sensor reports
            // no Y, which reads as X.
            let at = |i: usize| payload.get(i).copied().unwrap_or(0);
            let (x, y) = ([at(1), at(2)], [at(5), at(6)]);
            let y = if y == [0, 0] { x } else { y };
            out[1..5].copy_from_slice(&[x[0], x[1], y[0], y[1]]);
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
    ///   * output side — a LONG HID++ request report of [`LONG_LEN`] (20) bytes. This dialect only
    ///     emits LONG reports; a SHORT-only output pipe is recognized but stays unclaimed.
    ///   * input side — a standard SHORT or LONG maximum. Larger collections require a larger
    ///     read buffer and extended-report handling before they can be claimed.
    ///
    /// Byte-length convention (stated so the next reader doesn't re-derive it): `output_len`/
    /// `input_len` are `HIDP_CAPS` `OutputReportByteLength`/`InputReportByteLength`, which INCLUDE the
    /// leading report-id byte. So `SHORT_LEN` 7 / `LONG_LEN` 20 here are the FULL on-wire lengths
    /// (report id + payload: 7 = id+6, 20 = id+19), directly comparable to HID++'s SHORT/LONG message
    /// lengths — no ±1 report-id adjustment is needed on either side.
    ///
    /// Receivers are excluded ([`RECEIVER_PIDS`]): they answer on HID++ 1.0 registers / pairing slots
    /// `1..=6`, not the corded [`DEVICE_INDEX`] `0xFF` this dialect addresses — see the `DEVICE_INDEX` note.
    fn claims(&self, info: &HidDeviceInfo) -> bool {
        let output_supports_long_requests = info.output_len == LONG_LEN as u16;
        let input_carries_reply = info.input_len == SHORT_LEN || info.input_len == LONG_LEN as u16;
        info.vid == HIDPP_VID
            && !is_webcam_pid(info.pid)
            && output_supports_long_requests
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
        let start = Instant::now();
        let (index, _major, _minor) = find_device(t, is_receiver_pid(ctx.pid))?;
        let roundtrip_ms = start.elapsed().as_millis() as u64;

        // Enumerate the features we can reshape. `get_feature_index` maps "index 0" (device lacks it)
        // to `None`, so a mouse without a battery feature simply gets no battery command.
        let battery = get_feature_index(t, index, FEATURE_BATTERY)
            .map(|idx| (idx, FN_GET_BATTERY_STATUS))
            .or_else(|| get_feature_index(t, index, FEATURE_UNIFIED_BATTERY).map(|idx| (idx, FN_UNIFIED_GET_STATUS)));
        let dpi = get_feature_index(t, index, FEATURE_ADJUSTABLE_DPI)
            .map(|idx| (idx, FN_GET_SENSOR_DPI, vec![RESHAPE_DPI]))
            .or_else(|| {
                get_feature_index(t, index, FEATURE_EXTENDED_DPI)
                    .map(|idx| (idx, FN_EXTENDED_GET_SENSOR_DPI, vec![RESHAPE_DPI_XY, 0x00]))
            });
        // A receiver's USB product string names the receiver; the device names itself.
        let name = device_name(t, index).unwrap_or_else(|| hidpp_name(&ctx.product, ctx.pid));

        // One getter command per answered feature. `class` = the resolved feature INDEX (razer's
        // class slot), `id` = the function id, `size` = 0 (INERT: HID++ framing is fixed-shape, exec
        // ignores it), `args[0]` = the dialect-private reshape TAG (stripped before the wire, applied
        // to the reply). NO setters — writes without hardware to verify would break the no-unproven-
        // bytes rule (see the method doc); the reads are all this wave synthesizes.
        let mut commands: BTreeMap<String, CommandSpec> = BTreeMap::new();
        if let Some((idx, func)) = battery {
            commands.insert(
                "battery_level".into(),
                CommandSpec { class: idx, id: func, size: 0, args: vec![RESHAPE_BATTERY], transaction_id: None },
            );
        }
        if let Some((idx, func, args)) = dpi {
            commands.insert("dpi".into(), CommandSpec { class: idx, id: func, size: 0, args, transaction_id: None });
        }
        // The evidence set the mint seam records: exactly the getters that answered (== the command
        // map keys, since we only inserted answered features). `from_probe` debug-asserts the subset.
        let answered: Vec<String> = commands.keys().cloned().collect();

        let def = DeviceDef {
            name,
            codename: format!("auto-{:04x}", ctx.pid),
            dialect: "hidpp".into(),
            // Synthesized ⇒ Auto by construction (lands in devices/auto/, a reload re-stamps Auto).
            origin: DefOrigin::Auto,
            vendor_id: HIDPP_VID,
            // HID++ has no transaction ids; this slot carries the device index ([`address_of`]).
            transaction_id: if index == DEVICE_INDEX { 0 } else { index },
            // No streamed-lighting path in this dialect ⇒ nothing to pace.
            stream_wait_us: 0,
            modes: vec![Mode { name: "default".into(), product_id: ctx.pid }],
            event_alias_pids: Vec::new(),
            control_interface: ControlInterface {
                usage_page: ctx.usage_page,
                usage: ctx.usage,
                // Stored from the enumerated pipe for completeness, but HID++ MATCHING rides
                // `claims()` (VID + 20-byte LONG output shape), NOT the feature-report length —
                // HID++ uses output/input reports, so this length is informational here.
                feature_report_len: ctx.feature_len,
                interface: None,
            },
            commands,
            // Colour lighting, read from its features: per-key (0x8081) or zones (0x8070).
            lighting: probe_lighting(t, index),
            side_plates: None,
            side_plate_binds: std::collections::BTreeMap::new(),
            fn_binds: std::collections::BTreeMap::new(),
            // No push-report vocabulary probed this wave — an auto HID++ def carries none.
            events: None,
            buttons: Vec::new(),
            custody: None,
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
            Some(roundtrip_ms),
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
        let _ = size; // razer vocabulary; no meaning in HID++
        let index = address_of(transaction_id);

        // Hold the pipe's wire lock for the WHOLE conversation (write → read/drain): pair-atomicity
        // is the unit; between conversations other actors may interleave freely.
        let wire = t.wire_lock();
        let _wire = wire.as_ref().map(|w| w.acquire());

        let feature_idx = spec_class;
        let fn_id = spec_id;
        let reshape_tag = args.first().copied().unwrap_or(RESHAPE_NONE);
        let params = args.get(1..).unwrap_or(&[]); // strip the dialect-private tag byte

        let req = build_long(index, feature_idx, fn_id, params);
        t.write_output(&req)?;

        match await_reply(t, index, feature_idx, fn_id) {
            Answer::Reply(got) => {
                // 0x1000 reports level 0 for "unknown" (cpg-docs x1000), which is not an empty battery.
                if reshape_tag == RESHAPE_BATTERY && got[4] == 0 {
                    bail!("HID++ battery level unknown for feature {feature_idx:#06x}");
                }
                Ok(reshape(reshape_tag, &got[4..]))
            }
            Answer::Error { marker, code } => bail!(
                "HID++ error {code:#04x} ({}) for feature {feature_idx:#06x} fn {fn_id:#x}",
                err_name(marker, code)
            ),
            Answer::Silent => {
                bail!("HID++ timed out waiting for reply to feature {feature_idx:#06x} fn {fn_id:#x}")
            }
        }
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
    ) -> crate::dialect::Sent {
        let _ = size;
        let params = args.get(1..).unwrap_or(&[]);
        let req = build_long(address_of(transaction_id), class, id, params);
        let sent = t.write_output(&req).is_ok();
        if stream_wait_us > 0 {
            std::thread::sleep(std::time::Duration::from_micros(stream_wait_us));
        }
        if sent { crate::dialect::Sent::Unconfirmed } else { crate::dialect::Sent::Lost }
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
    /// Resolved feature INDEX for 0x1000 `BatteryLevelStatus`, when the device reports it.
    pub battery_feature: Option<u8>,
    /// Resolved feature INDEX for 0x2201 `AdjustableDPI`, when the device reports it.
    pub dpi_feature: Option<u8>,
}

/// Diagnostic probe of a live HID++ pipe: ping IRoot.getProtocolVersion (require >= 2.0), then
/// `IRoot.getFeature()` for battery (0x1000) and DPI (0x2201). Read-only — getters only, never a
/// blind write into an unknown framing. `None` if the pipe never answered a valid 2.0 ping (so it
/// is not a HID++ 2.0 pipe of ours). [`HidppDialect::probe`] now builds on this exact wire path to
/// synthesize a real def; this stays as the standalone READ-ONLY diagnostic (protocol + feature
/// indices, no def minted) that someone-with-hardware runs to confirm the spec against their device.
pub fn probe_report(t: &dyn Transport) -> Option<HidppProbe> {
    let (index, major, minor) = find_device(t, false).or_else(|| find_device(t, true))?;
    Some(HidppProbe {
        protocol_major: major,
        protocol_minor: minor,
        battery_feature: get_feature_index(t, index, FEATURE_BATTERY),
        dpi_feature: get_feature_index(t, index, FEATURE_ADJUSTABLE_DPI),
    })
}

/// getProtocolVersion at `index`: params [0x00, 0x00, pingData], the documented HID++ 1.0/2.0
/// disambiguation. `Some((major, minor))` for a 2.0 device that echoed the ping.
fn ping(t: &dyn Transport, index: u8) -> Option<(u8, u8)> {
    let req = build_long(index, IROOT_FEATURE, IROOT_FN_GET_PROTOCOL, &[0x00, 0x00, PING_TOKEN]);
    let wire = t.wire_lock();
    let _wire = wire.as_ref().map(|w| w.acquire());
    t.write_output(&req).ok()?;
    let reply = recv_matching(t, index, IROOT_FEATURE, IROOT_FN_GET_PROTOCOL)?;
    let (major, minor, echo) = (reply[4], reply[5], reply[6]);
    (echo == PING_TOKEN && major >= 0x02).then_some((major, minor))
}

/// The device a pipe reaches: the direct device at 0xFF, or a receiver's first pairing slot whose
/// device answers. `(index, major, minor)`.
fn find_device(t: &dyn Transport, receiver: bool) -> Option<(u8, u8, u8)> {
    if receiver {
        PAIRING_SLOTS.into_iter().find_map(|slot| ping(t, slot).map(|(a, b)| (slot, a, b)))
    } else {
        ping(t, DEVICE_INDEX).map(|(a, b)| (DEVICE_INDEX, a, b))
    }
}

/// The device's own name through DeviceName (0x0005): fn0 gives its length, fn1 a fragment from an
/// offset (Solaar `hidpp20.py` L1851-L1870 @e7304c4c). Control bytes are dropped, as for USB names.
fn device_name(t: &dyn Transport, index: u8) -> Option<String> {
    let feature = get_feature_index(t, index, FEATURE_DEVICE_NAME)?;
    let ask = |fn_id: u8, params: &[u8]| {
        let wire = t.wire_lock();
        let _wire = wire.as_ref().map(|w| w.acquire());
        t.write_output(&build_long(index, feature, fn_id, params)).ok()?;
        recv_matching(t, index, feature, fn_id)
    };
    let len = usize::from(ask(FN_NAME_COUNT, &[])?[4]).min(64);
    let mut name = Vec::with_capacity(len);
    while name.len() < len {
        let part = ask(FN_NAME_FRAGMENT, &[name.len() as u8])?;
        let take = (len - name.len()).min(LONG_LEN - 4);
        name.extend_from_slice(&part[4..4 + take]);
    }
    let name: String = String::from_utf8_lossy(&name).chars().filter(|c| !c.is_control()).collect();
    let name = name.trim();
    (!name.is_empty()).then(|| name.to_string())
}

/// IRoot.getFeature(featureId) → feature INDEX, or `None` when the device lacks it (index 0 is
/// reserved for `IRoot` itself, so 0 means "not present"). featureId is sent big-endian (MSB first).
fn get_feature_index(t: &dyn Transport, index: u8, feature_id: u16) -> Option<u8> {
    let params = [(feature_id >> 8) as u8, feature_id as u8];
    let req = build_long(index, IROOT_FEATURE, IROOT_FN_GET_FEATURE, &params);

    // ONE request/reply conversation under the pipe's wire lock (per-pair, like every probe site).
    let wire = t.wire_lock();
    let _wire = wire.as_ref().map(|w| w.acquire());

    t.write_output(&req).ok()?;
    let reply = recv_matching(t, index, IROOT_FEATURE, IROOT_FN_GET_FEATURE)?;
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

// ── lighting ─────────────────────────────────────────────────────────────────────────────────────
// Layouts from libratbag `src/hidpp20.c` @8235b5bb, Solaar `lib/logitech_receiver/hidpp20.py`,
// `settings_templates.py` and `rgb_power.py` @e7304c4c, and OpenRGB's `LogitechHIDPP20Controller`
// @5bcfa9ae. Only the writes those sources agree on are sent: per-key frames (0x8081) under 0x8071
// software control, and a zone's fixed colour (0x8070) with the persist byte 0, which keeps it out
// of flash. 0x8071's own effect write is never sent: the sources disagree on its persist byte.

pub const FEATURE_COLOR_LED_EFFECTS: u16 = 0x8070;
pub const FEATURE_RGB_EFFECTS: u16 = 0x8071;
pub const FEATURE_PER_KEY_LIGHTING_V2: u16 = 0x8081;
pub const FEATURE_ONBOARD_PROFILES: u16 = 0x8100;

const FN_8070_GET_INFO: u8 = 0x0;
const FN_8070_GET_ZONE_INFO: u8 = 0x1;
const FN_8070_GET_EFFECT_INFO: u8 = 0x2;
const FN_8070_SET_ZONE_EFFECT: u8 = 0x3;
const FN_8070_SET_SW_CONTROL: u8 = 0x8;
const FN_8070_GET_ZONE_EFFECT: u8 = 0xE;
const FN_8071_SW_CONTROL: u8 = 0x5;
const FN_8081_GET_INFO: u8 = 0x0;
const FN_8081_SET_INDIVIDUAL: u8 = 0x1;
const FN_8081_FRAME_END: u8 = 0x7;
const FN_8100_SET_MODE: u8 = 0x1;
const FN_8100_GET_MODE: u8 = 0x2;

/// 0x8070 extended capabilities: getZoneEffect reads back; the device ignores hue.
const EXT_HAS_ZONE_EFFECT: u16 = 0x0001;
const EXT_MONOCHROME_ONLY: u16 = 0x0008;
const EFFECT_FIXED: u16 = 0x0001;
/// Fixed colour's ramp byte: 2 = change at once, with no fade between frames.
const FIXED_NO_RAMP: u8 = 0x02;
const ONBOARD_MODE: u8 = 0x01;
const HOST_MODE: u8 = 0x02;
/// 0x8071 software control: set, mode 3 (the host owns the LEDs), flags effect (bit 0) and NV
/// config (bit 2); Solaar's documented call and OpenRGB's steady state. Released as mode 0.
const SW_CONTROL_TAKE: [u8; 3] = [0x01, 0x03, 0x05];
const SW_CONTROL_GIVE: [u8; 3] = [0x01, 0x00, 0x00];
/// 0x8081 key ids: the bitmap's three pages of 14 bytes, ids above 254 unaddressable.
const PERKEY_PAGES: u8 = 3;
const PERKEY_PAGE_BYTES: usize = 14;
const KEYS_PER_REPORT: usize = 4;
/// A key id is its HID Keyboard usage minus 3; G1-G5 and the logo sit outside that range.
const KEY_ID_USAGE_OFFSET: u16 = 3;
const KEY_ID_G1: u16 = 180;
const KEY_ID_LOGO: u16 = 210;
/// frameEnd answers Busy while the firmware is still taking keys; it is retried, not failed.
const BUSY: u8 = 0x08;
const FRAME_END_TRIES: u32 = 8;
const FRAME_END_BACKOFF: std::time::Duration = std::time::Duration::from_millis(10);
/// The row a per-key id with no keyboard cell takes in the LED map: it shows the frame's mean.
pub const NO_CELL: u16 = 255;

/// One request and its answer: `Ok(reply)`, `Err(Some(code))` for a HID++ error, `Err(None)` silent.
fn call(t: &dyn Transport, index: u8, feature: u8, fn_id: u8, params: &[u8]) -> std::result::Result<[u8; LONG_LEN], Option<u8>> {
    let wire = t.wire_lock();
    let _wire = wire.as_ref().map(|w| w.acquire());
    t.write_output(&build_long(index, feature, fn_id, params)).map_err(|_| None)?;
    match await_reply(t, index, feature, fn_id) {
        Answer::Reply(r) => Ok(r),
        Answer::Error { code, .. } => Err(Some(code)),
        Answer::Silent => Err(None),
    }
}

fn be16(hi: u8, lo: u8) -> u16 {
    u16::from_be_bytes([hi, lo])
}

/// The per-key ids a 0x8081 device lights, from its getInfo bitmap.
fn perkey_ids(t: &dyn Transport, index: u8, feature: u8) -> Vec<u16> {
    let mut ids = Vec::new();
    for page in 0..PERKEY_PAGES {
        let Ok(r) = call(t, index, feature, FN_8081_GET_INFO, &[0x00, page, 0x00]) else { break };
        for (byte, bits) in r[6..6 + PERKEY_PAGE_BYTES].iter().enumerate() {
            for bit in 0..8 {
                let id = u16::from(page) * 112 + (byte * 8 + bit) as u16;
                if bits & (1 << bit) != 0 && (1..=254).contains(&id) {
                    ids.push(id);
                }
            }
        }
    }
    ids
}

/// The keyboard cell a 0x8081 key id lights.
fn key_id_cell(id: u16) -> Option<(u8, u8)> {
    let name = match id {
        KEY_ID_LOGO => "LOGO",
        g if (KEY_ID_G1..KEY_ID_G1 + 5).contains(&g) => ["M1", "M2", "M3", "M4", "M5"][usize::from(g - KEY_ID_G1)],
        _ => crate::lighting::hid_usage_key(id + KEY_ID_USAGE_OFFSET)?,
    };
    crate::lighting::razer_key_cell(name)
}

/// Each 0x8070 zone's index of the fixed-colour effect, or `None` for a zone without one.
fn fixed_effect_indices(t: &dyn Transport, index: u8, feature: u8) -> Option<(Vec<Option<u8>>, u16)> {
    let info = call(t, index, feature, FN_8070_GET_INFO, &[]).ok()?;
    let zones = info[4];
    let ext = be16(info[7], info[8]);
    let mut fixed = Vec::new();
    for zone in 0..zones {
        let count = call(t, index, feature, FN_8070_GET_ZONE_INFO, &[zone]).map_or(0, |r| r[7]);
        let found = (0..count).find(|&i| {
            call(t, index, feature, FN_8070_GET_EFFECT_INFO, &[zone, i]).is_ok_and(|r| be16(r[6], r[7]) == EFFECT_FIXED)
        });
        fixed.push(found);
    }
    Some((fixed, ext))
}

/// What a HID++ device's lighting is, read-only: per-key on the keyboard grid (or one cell per key
/// for a device whose keys aren't keyboard keys), or one cell per colour zone. `None` without a
/// colour lighting feature neuron drives.
pub(crate) fn probe_lighting(t: &dyn Transport, index: u8) -> Option<crate::lighting::LightingDef> {
    use crate::lighting::{LightingDef, Protocol};
    let blank = |rows: u8, cols: u8, leds: Vec<[u16; 3]>| LightingDef {
        protocol: Protocol::Hidpp,
        rows,
        cols,
        varstore: 0,
        led_id: 0,
        effects: std::collections::BTreeMap::new(),
        custom_id: 0,
        effect: crate::registry::CommandSpec::default(),
        custom_frame: None,
        brightness: None,
        framing: crate::lighting::Framing::default(),
        display_args: None,
        channel: None,
        leds,
    };
    if let (Some(keys), Some(_)) = (
        get_feature_index(t, index, FEATURE_PER_KEY_LIGHTING_V2),
        get_feature_index(t, index, FEATURE_RGB_EFFECTS),
    ) {
        let ids = perkey_ids(t, index, keys);
        if !ids.is_empty() {
            let cells: Vec<Option<(u8, u8)>> = ids.iter().map(|&id| key_id_cell(id)).collect();
            if cells.iter().any(Option::is_some) {
                let leds = ids
                    .iter()
                    .zip(&cells)
                    .map(|(&id, c)| c.map_or([id, NO_CELL, 0], |(r, col)| [id, u16::from(r), u16::from(col)]))
                    .collect();
                return Some(blank(6, 22, leds));
            }
            let cols = ids.len().min(255) as u8;
            let leds = ids.iter().take(255).enumerate().map(|(i, &id)| [id, 0, i as u16]).collect();
            return Some(blank(1, cols, leds));
        }
    }
    let zones = get_feature_index(t, index, FEATURE_COLOR_LED_EFFECTS)?;
    let (fixed, ext) = fixed_effect_indices(t, index, zones)?;
    if ext & EXT_MONOCHROME_ONLY != 0 {
        return None;
    }
    let lit: Vec<u8> = (0..fixed.len() as u8).filter(|&z| fixed[usize::from(z)].is_some()).collect();
    if lit.is_empty() {
        return None;
    }
    let leds = lit.iter().enumerate().map(|(i, &z)| [u16::from(z), 0, i as u16]).collect();
    Some(blank(1, lit.len() as u8, leds))
}

/// How neuron holds a HID++ device's lighting.
enum Held {
    PerKey { keys: u8, rgb: u8 },
    Zones { feature: u8, fixed: Vec<Option<u8>>, readback: bool },
}

/// Neuron's hold on a HID++ device's lighting: the features it paints through, the colours it last
/// sent, and the onboard mode to put back.
pub(crate) struct ColourLink {
    index: u8,
    held: Held,
    sent: std::collections::HashMap<u16, crate::lighting::Rgb>,
    onboard: Option<(u8, u8)>,
    verified: bool,
}

/// Take the device's lighting: leave onboard mode when it is in it (the mode read first is kept to
/// put back), then take software control. No write here reaches the device's memory.
pub(crate) fn take_lighting(t: &dyn Transport, def: &crate::registry::DeviceDef) -> Result<ColourLink> {
    let index = address_of(def.transaction_id);
    let onboard = get_feature_index(t, index, FEATURE_ONBOARD_PROFILES).and_then(|f| {
        let mode = call(t, index, f, FN_8100_GET_MODE, &[]).ok()?[4];
        (mode == ONBOARD_MODE && call(t, index, f, FN_8100_SET_MODE, &[HOST_MODE]).is_ok()).then_some((f, mode))
    });
    let held = match (
        get_feature_index(t, index, FEATURE_PER_KEY_LIGHTING_V2),
        get_feature_index(t, index, FEATURE_RGB_EFFECTS),
    ) {
        (Some(keys), Some(rgb)) => {
            call(t, index, rgb, FN_8071_SW_CONTROL, &SW_CONTROL_TAKE)
                .map_err(|e| anyhow::anyhow!("software control refused ({})", e.map_or("silent", |c| err_name(ERR_HIDPP20, c))))?;
            Held::PerKey { keys, rgb }
        }
        _ => {
            let feature = get_feature_index(t, index, FEATURE_COLOR_LED_EFFECTS)
                .ok_or_else(|| anyhow::anyhow!("no colour lighting feature answered"))?;
            let (fixed, ext) = fixed_effect_indices(t, index, feature).ok_or_else(|| anyhow::anyhow!("zone info unanswered"))?;
            call(t, index, feature, FN_8070_SET_SW_CONTROL, &[0x01])
                .map_err(|e| anyhow::anyhow!("software control refused ({})", e.map_or("silent", |c| err_name(ERR_HIDPP20, c))))?;
            Held::Zones { feature, fixed, readback: ext & EXT_HAS_ZONE_EFFECT != 0 }
        }
    };
    Ok(ColourLink { index, held, sent: std::collections::HashMap::new(), onboard, verified: false })
}

impl ColourLink {
    /// Paint `(id, colour)` pairs: only those that changed since the last paint go out. Per-key
    /// frames commit with frameEnd; a zone's first colour is read back where the device can.
    pub(crate) fn paint(&mut self, t: &dyn Transport, colours: &[(u16, crate::lighting::Rgb)]) -> crate::dialect::Sent {
        use crate::dialect::Sent;
        let changed: Vec<(u16, crate::lighting::Rgb)> =
            colours.iter().copied().filter(|(id, c)| self.sent.get(id) != Some(c)).collect();
        if changed.is_empty() {
            return Sent::Taken;
        }
        let verdict = match &self.held {
            Held::PerKey { keys, .. } => {
                let keys = *keys;
                let mut verdict = Sent::Taken;
                for chunk in changed.chunks(KEYS_PER_REPORT) {
                    let params: Vec<u8> = chunk.iter().flat_map(|(id, c)| [*id as u8, c.r, c.g, c.b]).collect();
                    verdict = worst(verdict, sent_of(call(t, self.index, keys, FN_8081_SET_INDIVIDUAL, &params)));
                    if verdict == Sent::Lost {
                        break;
                    }
                }
                if verdict != Sent::Lost {
                    verdict = worst(verdict, self.frame_end(t, keys));
                }
                verdict
            }
            Held::Zones { feature, fixed, readback } => {
                let (feature, readback) = (*feature, *readback);
                let mut verdict = Sent::Taken;
                for &(zone, c) in &changed {
                    let Some(Some(effect)) = fixed.get(usize::from(zone)).copied() else { continue };
                    let mut params = [0u8; 13];
                    params[0] = zone as u8;
                    params[1] = effect;
                    params[2..5].copy_from_slice(&[c.r, c.g, c.b]);
                    params[5] = FIXED_NO_RAMP;
                    verdict = worst(verdict, sent_of(call(t, self.index, feature, FN_8070_SET_ZONE_EFFECT, &params)));
                    if readback && !self.verified && verdict == Sent::Taken {
                        let back = call(t, self.index, feature, FN_8070_GET_ZONE_EFFECT, &[zone as u8]);
                        if back.is_ok_and(|r| r[4] == zone as u8 && r[6..9] != [c.r, c.g, c.b]) {
                            verdict = Sent::Lost;
                        }
                        self.verified = true;
                    }
                }
                verdict
            }
        };
        if verdict == Sent::Taken {
            self.sent.extend(changed);
        } else {
            self.sent.clear();
        }
        verdict
    }

    fn frame_end(&self, t: &dyn Transport, keys: u8) -> crate::dialect::Sent {
        use crate::dialect::Sent;
        for _ in 0..FRAME_END_TRIES {
            match call(t, self.index, keys, FN_8081_FRAME_END, &[0u8; 16]) {
                Ok(_) => return Sent::Taken,
                Err(Some(BUSY)) => std::thread::sleep(FRAME_END_BACKOFF),
                Err(Some(_)) => return Sent::Lost,
                Err(None) => return Sent::Unconfirmed,
            }
        }
        Sent::Unconfirmed
    }

    /// Give the lighting back: release software control, then restore the onboard mode read at
    /// take. The device shows its own lighting again.
    pub(crate) fn release(&self, t: &dyn Transport) {
        match &self.held {
            Held::PerKey { rgb, .. } => {
                let _ = call(t, self.index, *rgb, FN_8071_SW_CONTROL, &SW_CONTROL_GIVE);
            }
            Held::Zones { feature, .. } => {
                let _ = call(t, self.index, *feature, FN_8070_SET_SW_CONTROL, &[0x00]);
            }
        }
        if let Some((feature, mode)) = self.onboard {
            let _ = call(t, self.index, feature, FN_8100_SET_MODE, &[mode]);
        }
    }
}

fn sent_of(r: std::result::Result<[u8; LONG_LEN], Option<u8>>) -> crate::dialect::Sent {
    use crate::dialect::Sent;
    match r {
        Ok(_) => Sent::Taken,
        Err(Some(BUSY)) | Err(None) => Sent::Unconfirmed,
        Err(Some(_)) => Sent::Lost,
    }
}

fn worst(a: crate::dialect::Sent, b: crate::dialect::Sent) -> crate::dialect::Sent {
    use crate::dialect::Sent;
    match (a, b) {
        (Sent::Lost, _) | (_, Sent::Lost) => Sent::Lost,
        (Sent::Unconfirmed, _) | (_, Sent::Unconfirmed) => Sent::Unconfirmed,
        _ => Sent::Taken,
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
        writes: Mutex<Vec<Vec<u8>>>,
        inbox: Mutex<VecDeque<Vec<u8>>>,
        padded: bool,
    }

    impl MockHidpp {
        fn with_replies(frames: Vec<Vec<u8>>) -> Self {
            MockHidpp {
                writes: Mutex::new(Vec::new()),
                inbox: Mutex::new(frames.into_iter().collect()),
                padded: false,
            }
        }
        fn last_write(&self) -> Vec<u8> {
            self.writes.lock().unwrap().last().expect("a request was written").clone()
        }
    }

    impl Transport for MockHidpp {
        fn padded_input_reports(&self) -> bool { self.padded }
        fn set_feature(&self, _buf: &[u8]) -> Result<()> {
            bail!("hidpp mock carries no feature reports")
        }
        fn get_feature(&self, _buf: &mut [u8]) -> Result<usize> {
            bail!("hidpp mock carries no feature reports")
        }
        fn write_output(&self, buf: &[u8]) -> Result<()> {
            self.writes.lock().unwrap().push(buf.to_vec());
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
        // The exact 20-byte getProtocolVersion request: [0x11, 0xFF, 0x00, 0x1C, 0x00, 0x00, ping].
        // 0x1C = (function 1 << 4) | softwareId 0x0C. Confirms build_long framing byte-for-byte.
        let mut want = [0u8; LONG_LEN];
        want[0] = 0x11; // long report id
        want[1] = 0xFF; // device index (receiver-direct default)
        want[2] = 0x00; // IRoot feature index
        want[3] = 0x1C; // (getProtocolVersion=1 << 4) | swId 0x0C
        want[4] = 0x00; // ping param byte 0
        want[5] = 0x00; // ping param byte 1
        want[6] = PING_TOKEN; // echoed ping token
        let built = build_long(DEVICE_INDEX, IROOT_FEATURE, IROOT_FN_GET_PROTOCOL, &[0x00, 0x00, PING_TOKEN]);
        assert_eq!(built, want, "getProtocolVersion request must be byte-identical to the golden");
    }

    #[test]
    fn probe_report_round_trips_protocol_and_features() {
        // Scripted: ping reply (major 0x04 = HID++ 2.0, echo 0x5A), then getFeature replies giving
        // battery feature index 0x06 and DPI feature index 0x07.
        let mock = MockHidpp::with_replies(vec![
            frame(&[0x11, 0xFF, 0x00, 0x1C, 0x04, 0x02, PING_TOKEN]), // getProtocolVersion reply
            frame(&[0x11, 0xFF, 0x00, 0x0C, 0x06]), // getFeature(0x1000) → index 6
            frame(&[0x11, 0xFF, 0x00, 0x0C, 0x07]), // getFeature(0x2201) → index 7
        ]);
        let report = probe_report(&mock).expect("valid 2.0 ping");
        assert_eq!(report.protocol_major, 0x04);
        assert_eq!(report.protocol_minor, 0x02);
        assert_eq!(report.battery_feature, Some(0x06));
        assert_eq!(report.dpi_feature, Some(0x07));
        // The FIRST thing written must be the exact ping golden.
        let first = mock.writes.lock().unwrap()[0].clone();
        assert_eq!(first.len(), LONG_LEN, "probe emits a full advertised LONG output report");
        assert_eq!(first[..7], [0x11, 0xFF, 0x00, 0x1C, 0x00, 0x00, PING_TOKEN]);
    }

    #[test]
    fn error_report_maps_to_error_with_code() {
        // exec for feature 0x06 fn 0; the device answers with a 2.0 error, code 0x02 (InvalidArgument).
        let mock = MockHidpp::with_replies(vec![frame(&[
            0x11, 0xFF, ERR_HIDPP20, 0x06, func_swid(0x00), 0x02,
        ])]);
        let err = HidppDialect
            .exec(&mock, 0x00, 0x06, 0x00, 0x00, &[RESHAPE_NONE])
            .expect_err("an error report must map to Err");
        let msg = format!("{err}");
        assert!(msg.contains("InvalidArgument"), "error names the code: {msg}");
    }

    #[test]
    fn hidpp10_errors_use_their_own_names() {
        // 1.0 code 0x08 is UnknownDevice; the 2.0 table would call it Busy.
        let mock = MockHidpp::with_replies(vec![vec![
            0x10, 0xFF, ERR_HIDPP10, 0x06, func_swid(0x00), 0x08, 0,
        ]]);
        let msg = format!("{}", HidppDialect.exec(&mock, 0, 0x06, 0x00, 0, &[RESHAPE_NONE]).unwrap_err());
        assert!(msg.contains("UnknownDevice"), "{msg}");
    }

    #[test]
    fn another_clients_error_is_not_ours() {
        // G HUB (swId 0x0D) gets an error just ahead of our reply on the same device.
        let foreign = frame(&[0x11, 0xFF, ERR_HIDPP20, 0x06, 0x0D, 0x08]);
        let ours = frame(&[0x11, 0xFF, 0x07, func_swid(0x02), 0x00, 0x03, 0x20]);
        let mock = MockHidpp::with_replies(vec![foreign, ours]);
        let out = HidppDialect.exec(&mock, 0, 0x07, 0x02, 0, &[RESHAPE_DPI]).expect("our reply");
        assert_eq!(&out[1..3], &[0x03, 0x20]);
    }

    #[test]
    fn bluetooth_reply_on_index_zero_is_accepted() {
        let mock = MockHidpp::with_replies(vec![frame(&[0x11, 0x00, 0x07, func_swid(0x02), 0x00, 0x03, 0x20])]);
        assert!(HidppDialect.exec(&mock, 0, 0x07, 0x02, 0, &[RESHAPE_DPI]).is_ok());
    }

    #[test]
    fn notifications_ahead_of_the_reply_do_not_end_the_wait() {
        // Eleven notifications (swId 0) ahead of our reply: a frame count would give up first.
        let mut frames: Vec<Vec<u8>> = (0..11).map(|_| frame(&[0x11, 0xFF, 0x04, 0x00, 0x55])).collect();
        frames.push(frame(&[0x11, 0xFF, 0x07, func_swid(0x02), 0x00, 0x03, 0x20]));
        let mock = MockHidpp::with_replies(frames);
        assert!(HidppDialect.exec(&mock, 0, 0x07, 0x02, 0, &[RESHAPE_DPI]).is_ok());
    }

    #[test]
    fn an_unknown_battery_level_is_not_zero_percent() {
        let mock = MockHidpp::with_replies(vec![frame(&[0x11, 0xFF, 0x06, func_swid(0x00), 0, 0, 0])]);
        let err = HidppDialect.exec(&mock, 0, 0x06, 0x00, 0, &[RESHAPE_BATTERY]).unwrap_err();
        assert!(format!("{err}").contains("unknown"), "{err}");
    }

    #[test]
    fn a_mouse_with_only_the_newer_features_still_gets_battery_and_dpi() {
        // ping, 0x1000 absent, 0x1004 at index 8, 0x2201 absent, 0x2202 at index 9; then the reads.
        let mock = MockHidpp::with_replies(vec![
            frame(&[0x11, 0xFF, 0x00, func_swid(IROOT_FN_GET_PROTOCOL), 0x04, 0x02, PING_TOKEN]),
            frame(&[0x11, 0xFF, 0x00, func_swid(IROOT_FN_GET_FEATURE), 0x00]),
            frame(&[0x11, 0xFF, 0x00, func_swid(IROOT_FN_GET_FEATURE), 0x08]),
            frame(&[0x11, 0xFF, 0x00, func_swid(IROOT_FN_GET_FEATURE), 0x00]),
            frame(&[0x11, 0xFF, 0x00, func_swid(IROOT_FN_GET_FEATURE), 0x09]),
            frame(&[0x11, 0xFF, 0x00, func_swid(IROOT_FN_GET_FEATURE), 0x00]),
            // the lighting features 0x8081, 0x8071 and 0x8070: absent
            frame(&[0x11, 0xFF, 0x00, func_swid(IROOT_FN_GET_FEATURE), 0x00]),
            frame(&[0x11, 0xFF, 0x00, func_swid(IROOT_FN_GET_FEATURE), 0x00]),
            frame(&[0x11, 0xFF, 0x00, func_swid(IROOT_FN_GET_FEATURE), 0x00]),
            frame(&[0x11, 0xFF, 0x08, func_swid(FN_UNIFIED_GET_STATUS), 64, 0x04, 0x00]),
            frame(&[0x11, 0xFF, 0x09, func_swid(FN_EXTENDED_GET_SENSOR_DPI), 0x00, 0x06, 0x40, 0x03, 0x20, 0x03, 0x20]),
        ]);
        let s = HidppDialect.probe(&mock, &hidpp_ctx("Logitech G Pro X Superlight")).expect("adopted");
        let bat = s.def.command("battery_level").expect("battery from 0x1004");
        assert_eq!((bat.class, bat.id), (0x08, FN_UNIFIED_GET_STATUS));
        let dpi = s.def.command("dpi").expect("dpi from 0x2202");
        assert_eq!((dpi.class, dpi.id), (0x09, FN_EXTENDED_GET_SENSOR_DPI));
        let out = HidppDialect.exec(&mock, 0, bat.class, bat.id, 0, &bat.args).unwrap();
        assert_eq!((u32::from(out[1]) * 100 + 127) / 255, 64, "64% charge");
        let out = HidppDialect.exec(&mock, 0, dpi.class, dpi.id, 0, &dpi.args).unwrap();
        assert_eq!(&out[1..5], &[0x06, 0x40, 0x03, 0x20], "X 1600, Y 800 read separately");
        let sent = mock.writes.lock().unwrap();
        assert_eq!(sent.last().unwrap()[4], 0x00, "the DPI read asks sensor 0");
    }

    #[test]
    fn a_single_axis_extended_dpi_reads_y_as_x() {
        let out = reshape(RESHAPE_DPI_XY, &[0x00, 0x03, 0x20, 0x03, 0x20, 0x00, 0x00]);
        assert_eq!(&out[1..5], &[0x03, 0x20, 0x03, 0x20]);
    }

    #[test]
    fn webcams_are_never_claimed() {
        let cam = HidDeviceInfo { pid: 0x085E, ..info(HIDPP_VID, LONG_LEN as u16, LONG_LEN as u16) };
        assert!(!HidppDialect.claims(&cam));
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
        let request = mock.last_write();
        assert_eq!(request.len(), LONG_LEN, "exec emits a full LONG output report");
        assert_eq!(request[..4], [0x11, 0xFF, 0x06, func_swid(0x00)]);
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
        assert!(!HidppDialect.claims(&info(HIDPP_VID, 7, 20)), "short-only output is gated because this dialect writes long requests");
        assert!(!HidppDialect.claims(&info(HIDPP_VID, 7, 7)), "short-only bidirectional pipe is gated until short framing exists");
        assert!(HidppDialect.claims(&info(HIDPP_VID, 20, 7)), "a short input maximum carries standard replies");
        assert!(!HidppDialect.claims(&info(HIDPP_VID, 20, 64)), "a 64-byte input maximum exceeds the read buffer");
        assert!(!HidppDialect.claims(&info(HIDPP_VID, 20, 8)), "nonstandard input maximum is not claimed");
        // One-direction collections can never complete write_output → read_input:
        assert!(!HidppDialect.claims(&info(HIDPP_VID, 20, 0)), "output-only (no input report) can't carry a reply");
        assert!(!HidppDialect.claims(&info(HIDPP_VID, 7, 0)), "output-only short pipe is unsupported and can't carry a reply");
        assert!(!HidppDialect.claims(&info(HIDPP_VID, 0, 20)), "input-only (no output report) can't send a request");
        // Wrong report shape / wrong vendor:
        assert!(!HidppDialect.claims(&info(HIDPP_VID, 64, 64)), "Logitech + 64-byte report ≠ HID++");
        assert!(!HidppDialect.claims(&info(0x1532, 91, 0)), "Razer VID ≠ HID++");
    }

    #[test]
    fn receivers_are_claimed_and_reached_through_their_pairing_slots() {
        for pid in [0xC52B, 0xC548, 0xC545] {
            assert!(HidppDialect.claims(&info_pid(HIDPP_VID, pid, LONG_LEN as u16, LONG_LEN as u16)), "{pid:04x}");
        }
        // Slot 1 is empty (the receiver answers a 1.0 error), slot 2 holds a mouse that names itself.
        let ping_fn = func_swid(IROOT_FN_GET_PROTOCOL);
        let feat_fn = func_swid(IROOT_FN_GET_FEATURE);
        let mut name = vec![0x11, 0x02, 0x05, func_swid(FN_NAME_FRAGMENT)];
        name.extend_from_slice(b"G502 X LIGHTSPEED");
        let mock = MockHidpp::with_replies(vec![
            vec![0x10, 0x01, ERR_HIDPP10, IROOT_FEATURE, ping_fn, 0x08, 0x00],
            frame(&[0x11, 0x02, 0x00, ping_fn, 0x04, 0x02, PING_TOKEN]),
            frame(&[0x11, 0x02, 0x00, feat_fn, 0x06]), // 0x1000
            frame(&[0x11, 0x02, 0x00, feat_fn, 0x07]), // 0x2201
            frame(&[0x11, 0x02, 0x00, feat_fn, 0x05]), // 0x0005 DeviceName at index 5
            frame(&[0x11, 0x02, 0x05, func_swid(FN_NAME_COUNT), 17]),
            frame(&name[..LONG_LEN.min(name.len())]),
            frame(&[0x11, 0x02, 0x05, func_swid(FN_NAME_FRAGMENT), b'D']),
            // the lighting features 0x8081, 0x8071 and 0x8070: absent
            frame(&[0x11, 0x02, 0x00, feat_fn, 0x00]),
            frame(&[0x11, 0x02, 0x00, feat_fn, 0x00]),
            frame(&[0x11, 0x02, 0x00, feat_fn, 0x00]),
            frame(&[0x11, 0x02, 0x06, func_swid(FN_GET_BATTERY_STATUS), 50]),
        ]);
        let ctx = crate::synth::SynthCtx { pid: 0xC547, product: "USB Receiver".into(), ..hidpp_ctx("") };
        let s = HidppDialect.probe(&mock, &ctx).expect("adopted through slot 2");
        assert_eq!(s.def.transaction_id, 2, "the def carries the slot");
        assert_eq!(s.def.name, "G502 X LIGHTSPEED", "named by the device, not the receiver");
        let bat = s.def.command("battery_level").unwrap();
        let out = HidppDialect.exec(&mock, s.def.transaction_id, bat.class, bat.id, 0, &bat.args).unwrap();
        assert_eq!((u32::from(out[1]) * 100 + 127) / 255, 50);
        let sent = mock.writes.lock().unwrap();
        assert_eq!(sent[0][1], 0x01, "slot 1 asked first");
        assert!(sent[1..].iter().all(|w| w[1] == 0x02), "everything after addresses slot 2");
    }

    #[test]
    fn a_receiver_with_nothing_awake_is_not_adopted_yet() {
        let ping_fn = func_swid(IROOT_FN_GET_PROTOCOL);
        let errs = (1..=6).map(|slot| vec![0x10, slot, ERR_HIDPP10, IROOT_FEATURE, ping_fn, 0x09, 0x00]).collect();
        let mock = MockHidpp::with_replies(errs);
        let ctx = crate::synth::SynthCtx { pid: 0xC548, ..hidpp_ctx("") };
        assert!(HidppDialect.probe(&mock, &ctx).is_none());
        assert_eq!(mock.writes.lock().unwrap().len(), 6, "one ping per slot");
    }

    /// A minimal hidpp-tagged `DeviceDef` with a chosen control usage pair (`feature_report_len` is
    /// deliberately 0 — meaningless for HID++). `vendor_id` 1133 = 0x046D.
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
        // HID++ control matching = claims() (VID + 20-byte LONG output shape) AND the stored usage pair;
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
            !def.matches_control(&pipe(0x0001, 0x0002, 7, 20)),
            "matching usage but a short-only output shape = not claimed at all"
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
            keyboard: false,
        }
    }

    #[test]
    fn probe_synthesizes_a_hidpp_def_and_battery_round_trips() {
        use crate::registry::DefOrigin;
        // Scripted: 2.0 ping, battery feature idx 6, DPI feature idx 7, then a battery reply for the
        // exec below (the mock inbox is FIFO — probe drains the first three, exec drains the fourth).
        let mock = MockHidpp::with_replies(vec![
            frame(&[0x11, 0xFF, 0x00, 0x1C, 0x04, 0x02, PING_TOKEN]), // getProtocolVersion → 4.2
            frame(&[0x11, 0xFF, 0x00, 0x0C, 0x06]),                   // getFeature(0x1000) → idx 6
            frame(&[0x11, 0xFF, 0x00, 0x0C, 0x07]),                   // getFeature(0x2201) → idx 7
            frame(&[0x11, 0xFF, 0x00, 0x0C, 0x00]),                   // getFeature(0x0005) → absent
            frame(&[0x11, 0xFF, 0x00, 0x0C, 0x00]),                   // getFeature(0x8081) → absent
            frame(&[0x11, 0xFF, 0x00, 0x0C, 0x00]),                   // getFeature(0x8071) → absent
            frame(&[0x11, 0xFF, 0x00, 0x0C, 0x00]),                   // getFeature(0x8070) → absent
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
        let pct = (u32::from(out[1]) * 100 + 127) / 255;
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
            frame(&[0x11, 0xFF, 0x00, 0x1C, 0x04, 0x02, PING_TOKEN]),
            frame(&[0x11, 0xFF, 0x00, 0x0C, 0x06]),
            frame(&[0x11, 0xFF, 0x00, 0x0C, 0x07]),
            frame(&[0x11, 0xFF, 0x00, 0x0C, 0x00]),
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

    #[test]
    fn padded_short_reports_require_an_explicit_transport_envelope() {
        let mut reply = frame(&[0x10, 0xFF, 7, func_swid(2), 0, 3, 0x20]);
        reply[7..].fill(0xA5);
        let exact = MockHidpp::with_replies(vec![reply.clone()]);
        assert!(HidppDialect.exec(&exact, 0, 7, 2, 0, &[RESHAPE_DPI]).is_err());

        let mut padded = MockHidpp::with_replies(vec![reply.clone(), reply]);
        padded.padded = true;
        let dpi = HidppDialect.exec(&padded, 0, 7, 2, 0, &[RESHAPE_DPI]).unwrap();
        assert_eq!(&dpi[1..5], &[3, 0x20, 3, 0x20]);
        let raw = HidppDialect.exec(&padded, 0, 7, 2, 0, &[RESHAPE_NONE]).unwrap();
        assert_eq!(&raw[..3], &[0, 3, 0x20]);
        assert!(raw[3..].iter().all(|b| *b == 0), "OS envelope bytes are not protocol payload");

        let mut probe = MockHidpp::with_replies(vec![
            frame(&[0x10, 0xFF, 0, 0x1C, 2, 0, PING_TOKEN]),
            frame(&[0x10, 0xFF, 0, 0x0C, 6, 0, 0]),
            frame(&[0x10, 0xFF, 0, 0x0C, 7, 0, 0]),
        ]);
        probe.padded = true;
        let report = probe_report(&probe).unwrap();
        assert_eq!(report.battery_feature, Some(6));
        assert_eq!(report.dpi_feature, Some(7));
    }

    #[test]
    fn reply_matching_requires_a_complete_report_for_its_id() {
        let mut short = [0u8; SHORT_LEN as usize];
        short[..4].copy_from_slice(&[REPORT_ID_SHORT, DEVICE_INDEX, 7, func_swid(2)]);
        assert!(valid_report_shape(&short));
        assert!(matches_reply(&short, DEVICE_INDEX, 7, 2));
        assert!(!matches_reply(&short[..SHORT_LEN as usize - 1], DEVICE_INDEX, 7, 2));

        let mut long = [0u8; LONG_LEN];
        long[..4].copy_from_slice(&[REPORT_ID_LONG, DEVICE_INDEX, 7, func_swid(2)]);
        assert!(valid_report_shape(&long));
        assert!(matches_reply(&long, DEVICE_INDEX, 7, 2));
        assert!(!matches_reply(&long[..LONG_LEN - 1], DEVICE_INDEX, 7, 2));
        assert!(!matches_reply(&long[..4], DEVICE_INDEX, 7, 2));

        let truncated_error = [REPORT_ID_LONG, DEVICE_INDEX, ERR_HIDPP20, 0, 0, 9];
        assert_eq!(error_for(&truncated_error, DEVICE_INDEX, 0, 0), None);
    }

    // ---- Property tests (EXPERIMENTAL dialect — see module doc: spec-implemented, zero
    // hardware verification). These properties pin the STRUCTURAL contracts that follow from the
    // code as written — reshape's `.unwrap_or(0)`/`.get(..)` semantics, the fixed 80-byte output
    // shape, the length guards on `error_for`/`matches_reply`, and the wire-length constants —
    // not the truth of the underlying Logitech spec, which nothing here can hardware-verify.
    mod props {
        use super::*;
        use proptest::prelude::*;

        fn cfg() -> ProptestConfig {
            ProptestConfig { cases: 256, ..ProptestConfig::default() }
        }

        proptest! {
            #![proptest_config(cfg())]

            /// (a) `reshape` never panics for ANY tag and ANY payload length 0..64, and its output
            /// is always exactly the documented 80-byte arg-layout shape (fixed-size array, so this
            /// is really pinning "no panic" — the shape is a compile-time fact of the return type,
            /// restated here so a future signature change trips the test).
            #[test]
            fn reshape_never_panics_on_arbitrary_payloads(
                tag in any::<u8>(),
                payload in proptest::collection::vec(any::<u8>(), 0..64),
            ) {
                let out = reshape(tag, &payload);
                prop_assert_eq!(out.len(), 80, "reshape's documented arg-layout shape is fixed at 80 bytes");
            }

            /// (b) RESHAPE_BATTERY structural contract (hidpp.rs:237-238): a payload with NO byte
            /// at index 0 (`payload.first().copied().unwrap_or(0)`) must decode as pct=0, i.e.
            /// `out[1] == 0` — the "empty payload reads as unknown/0%" default, never a garbage
            /// byte borrowed from elsewhere. Whenever a first byte DOES exist, out[1] is exactly
            /// the documented `(pct*255+50)/100` scale-up, clamped to 255 (pct is a raw u8 so the
            /// clamp is unreachable in practice, but the property pins the formula regardless).
            #[test]
            fn battery_reshape_truncated_payload_yields_zero_not_garbage(
                payload in proptest::collection::vec(any::<u8>(), 0..64),
            ) {
                let out = reshape(RESHAPE_BATTERY, &payload);
                let expected = match payload.first().copied() {
                    None => 0u8,
                    Some(pct) => ((u32::from(pct) * 255 + 50) / 100).min(255) as u8,
                };
                prop_assert_eq!(out[1], expected);
                // Every other byte of the 80-byte output stays untouched-zero — RESHAPE_BATTERY
                // writes ONLY out[1] (hidpp.rs:238).
                prop_assert!(out[0] == 0 && out[2..].iter().all(|&b| b == 0));
            }

            /// (b) RESHAPE_DPI structural contract (hidpp.rs:244-249): `payload.get(1)`/`get(2)`
            /// each `.unwrap_or(0)` independently — a payload too short to hold byte 1 and/or byte 2
            /// must decode those fields as 0, NOT borrow an adjacent byte (e.g. a 1-byte payload
            /// must NOT let payload[0] leak into the "hi" slot). The single DPI value is always
            /// mirrored identically onto both X (out[1..3]) and Y (out[3..5]).
            #[test]
            fn dpi_reshape_truncated_payload_yields_zeroed_fields_not_adjacent_bytes(
                payload in proptest::collection::vec(any::<u8>(), 0..64),
            ) {
                let out = reshape(RESHAPE_DPI, &payload);
                let hi = payload.get(1).copied().unwrap_or(0);
                let lo = payload.get(2).copied().unwrap_or(0);
                prop_assert_eq!([out[1], out[2], out[3], out[4]], [hi, lo, hi, lo]);
                prop_assert_eq!(out[0], 0, "the reshape tag slot is never populated by RESHAPE_DPI");
            }
            /// (b) RESHAPE_NONE (and any unrecognized tag — the `_` arm, hidpp.rs:251-255): the raw
            /// payload is copied left-aligned into out[0..], truncated to 80 bytes, with everything
            /// past the payload's length left zeroed. Covers both the documented RESHAPE_NONE tag
            /// and arbitrary unknown tags, since they share the same `_` arm.
            #[test]
            fn none_and_unknown_tag_reshape_copies_payload_left_aligned_and_zero_pads(
                tag in any::<u8>().prop_filter("not a recognized tag", |t| ![RESHAPE_BATTERY, RESHAPE_DPI, RESHAPE_DPI_XY].contains(t)),
                payload in proptest::collection::vec(any::<u8>(), 0..64),
            ) {
                let out = reshape(tag, &payload);
                let n = payload.len().min(80);
                prop_assert_eq!(&out[..n], &payload[..n]);
                prop_assert!(out[n..].iter().all(|&b| b == 0), "bytes past the payload stay zero-padded");
            }

            /// (c) `error_for`/`matches_reply` accept only complete HID++ report shapes:
            /// short 0x10 reports are 7 bytes and long 0x11 reports are 20 bytes. Arbitrary
            /// slices must remain length-safe and cannot be mistaken for a complete reply.
            #[test]
            fn error_for_and_matches_reply_are_length_safe(
                full in proptest::collection::vec(any::<u8>(), 0..24),
                cut_at in 0usize..24,
                feature_idx in any::<u8>(),
                fn_id in any::<u8>(),
            ) {
                let cut = cut_at.min(full.len());
                let buf = &full[..cut];

                let ec = error_for(buf, DEVICE_INDEX, feature_idx, fn_id); // must not panic

                let mr = matches_reply(buf, DEVICE_INDEX, feature_idx, fn_id); // must not panic
                if !valid_report_shape(buf) {
                    prop_assert_eq!(ec, None, "truncated or overlong frames are not error reports");
                    prop_assert!(!mr, "truncated or overlong frames are not normal replies");
                }
            }

            /// (d) Golden-consistency: `build_long` for ARBITRARY feature/fn/params never exceeds
            /// [`LONG_LEN`] — the wire limit this dialect always frames to (module doc: "We frame
            /// everything as LONG reports"). Params past the 16-byte body are silently dropped
            /// (hidpp.rs:153-157: `if 4 + i < LONG_LEN`), never overflowing the frame or panicking,
            /// for a params slice of ANY length. Also pins SHORT_LEN <= LONG_LEN, the ordering the
            /// module doc's frame table asserts (7-byte short, 20-byte long).
            #[test]
            fn build_long_request_never_exceeds_the_long_wire_limit(
                feature_idx in any::<u8>(),
                fn_id in any::<u8>(),
                params in proptest::collection::vec(any::<u8>(), 0..64),
            ) {
                let req = build_long(DEVICE_INDEX, feature_idx, fn_id, &params);
                prop_assert_eq!(req.len(), LONG_LEN, "build_long's frame is pinned at the LONG_LEN wire limit");
                prop_assert!((SHORT_LEN as usize) <= LONG_LEN, "SHORT_LEN must never exceed LONG_LEN");
                // Header bytes are exactly as build_long documents, for every input.
                prop_assert_eq!(req[0], REPORT_ID_LONG);
                prop_assert_eq!(req[1], DEVICE_INDEX);
                prop_assert_eq!(req[2], feature_idx);
                prop_assert_eq!(req[3], func_swid(fn_id));
            }
        }
    }
}

#[cfg(test)]
#[path = "hidpp_lighting_tests.rs"]
mod lighting_tests;
