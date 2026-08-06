// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

//! The Razer vendor wire format: the 90-byte `razer_report`, its CRC, and status codes.
//!
//! Framing (verified live): a HID *feature report* of 91 bytes = 1 report-id byte (0x00)
//! followed by the 90-byte report. CRC = XOR of report bytes [2..=87], i.e. buffer
//! bytes [3..=88], stored at buffer[89]. Reserved trailing byte = 0.

/// Length of the on-wire report (excludes the leading HID report-id byte).
pub const REPORT_LEN: usize = 90;
/// Full HID feature buffer length (report-id byte + report).
pub const BUF_LEN: usize = REPORT_LEN + 1;

/// Device-side processing status returned in the reply.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    New,
    Busy,
    Success,
    Fail,
    Timeout,
    Unsupported,
    Other(u8),
}

impl Status {
    pub fn from_u8(v: u8) -> Self {
        match v {
            0 => Status::New,
            1 => Status::Busy,
            2 => Status::Success,
            3 => Status::Fail,
            4 => Status::Timeout,
            5 => Status::Unsupported,
            o => Status::Other(o),
        }
    }
}

/// A decoded Razer report. Argument payload is the 80-byte body.
#[derive(Clone, Copy)]
pub struct Report {
    pub status: u8,
    pub transaction_id: u8,
    pub data_size: u8,
    pub class: u8,
    pub id: u8,
    pub args: [u8; 80],
}

impl Report {
    /// Build an outgoing command (status 0, no remaining packets, zero args).
    pub fn command(transaction_id: u8, class: u8, id: u8, size: u8) -> Self {
        Report {
            status: 0,
            transaction_id,
            data_size: size,
            class,
            id,
            args: [0; 80],
        }
    }

    /// Serialize into the 91-byte HID feature buffer (with report-id and CRC).
    pub fn to_buf(&self) -> [u8; BUF_LEN] {
        let mut b = [0u8; BUF_LEN];
        b[0] = 0x00; // HID report id
        b[1] = self.status; // report[0]
        b[2] = self.transaction_id; // report[1]
                                    // b[3..=4] remaining_packets = 0; b[5] protocol_type = 0
        b[6] = self.data_size; // report[5]
        b[7] = self.class; // report[6]
        b[8] = self.id; // report[7]
        b[9..89].copy_from_slice(&self.args); // report[8..=87]
        b[89] = crc(&b); // report[88]
                         // b[90] reserved = 0
        b
    }

    /// Parse a 91-byte reply buffer.
    pub fn from_buf(b: &[u8; BUF_LEN]) -> Self {
        let mut args = [0u8; 80];
        args.copy_from_slice(&b[9..89]);
        Report {
            status: b[1],
            transaction_id: b[2],
            data_size: b[6],
            class: b[7],
            id: b[8],
            args,
        }
    }

    pub fn status(&self) -> Status {
        Status::from_u8(self.status)
    }
}

/// Razer CRC: XOR of buffer bytes [3..=88].
pub fn crc(buf: &[u8; BUF_LEN]) -> u8 {
    buf[3..=88].iter().fold(0u8, |c, &b| c ^ b)
}

/// Read one reply frame's status IF it echoes the awaited command — the shared echo filter
/// (b[7]==class && b[8]==id → Status from b[1]) that device/discover/synth exec loops each
/// hand-rolled. One frame vocabulary; the loops keep their own PACING (see docs/TDD.md §5.7:
/// cadence differences are deliberate calibration behavior, not accidents). `None` means the
/// buffer is not (yet) our reply — cross-talk from another command, so keep polling.
///
/// Takes a SLICE, not `&[u8; BUF_LEN]`: offsets 1/7/8 sit at the SAME place in every razer-family
/// envelope this codebase speaks — the 91-byte razer_report buffer AND the razer-audio dialect's
/// 64-byte envelope ([`crate::dialect::RazerAudioDialect`], HARDWARE FACTS-verified on the Seiren
/// V3 Mini 2026-07-08) — so one echo filter serves both instead of each dialect hand-rolling its
/// own copy. Every existing caller passes `&[u8; BUF_LEN]`, which coerces to `&[u8]` at the call
/// site — zero behavior change for razer_report.
///
/// Relaxing the parameter from `&[u8; BUF_LEN]` to `&[u8]` gave up the compile-time length proof, so
/// a runtime guard restores it: a buffer too short to hold offsets 1/7/8 is `None` (not our reply,
/// keep polling) — a truncated read can never panic here.
pub fn reply_status(b: &[u8], class: u8, id: u8) -> Option<Status> {
    if b.len() <= 8 {
        return None;
    }
    (b[7] == class && b[8] == id).then(|| Status::from_u8(b[1]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_and_crc() {
        let r = Report::command(0x1F, 0x00, 0x81, 0x02);
        let buf = r.to_buf();
        assert_eq!(buf[0], 0x00);
        assert_eq!(buf[2], 0x1F);
        assert_eq!(buf[6], 0x02);
        assert_eq!(buf[7], 0x00);
        assert_eq!(buf[8], 0x81);
        // crc over [3..=88]; for this command only buf[6],[7],[8] are nonzero
        assert_eq!(buf[89], 0x02 ^ 0x81); // crc = data_size ^ class(0x00) ^ id
        let back = Report::from_buf(&buf);
        assert_eq!(back.class, 0x00);
        assert_eq!(back.id, 0x81);
        assert_eq!(back.data_size, 0x02);
    }

    #[test]
    fn reply_status_is_the_echo_filter() {
        // A SUCCESS reply that echoes the awaited class/id yields its status…
        let mut b = Report::command(0x1F, 0x04, 0x85, 0x07).to_buf();
        b[1] = 0x02; // status = Success in the reply
        assert_eq!(reply_status(&b, 0x04, 0x85), Some(Status::Success));
        // …a Fail status still round-trips (the caller decides terminal vs keep-polling)…
        b[1] = 0x03;
        assert_eq!(reply_status(&b, 0x04, 0x85), Some(Status::Fail));
        // …but a class OR id mismatch is cross-talk, not our reply → None (keep polling).
        assert_eq!(reply_status(&b, 0x04, 0x86), None, "id mismatch");
        assert_eq!(reply_status(&b, 0x00, 0x85), None, "class mismatch");
        // …and a truncated slice (offsets 1/7/8 out of range) is None, never a panic — the runtime
        // guard that replaces the lost `&[u8; BUF_LEN]` compile-time length proof.
        assert_eq!(reply_status(&b[..8], 0x04, 0x85), None, "too short to hold offset 8");
        assert_eq!(reply_status(&[], 0x04, 0x85), None, "empty slice");
    }

    /// `to_buf`/`from_buf` must round-trip EVERY field losslessly — not just class/id/data_size
    /// (already covered by `roundtrip_and_crc`), but status, transaction_id, and the full 80-byte
    /// args payload at its exact offsets, since a shifted or truncated arg copy would silently
    /// corrupt device commands.
    #[test]
    fn roundtrip_preserves_status_tx_id_and_full_args_payload() {
        let mut args = [0u8; 80];
        for (i, b) in args.iter_mut().enumerate() {
            *b = i as u8; // distinct per-index values so a shift/truncation is detectable
        }
        let r = Report {
            status: 0x02,
            transaction_id: 0x3F,
            data_size: 0x50,
            class: 0x0B,
            id: 0x85,
            args,
        };
        let buf = r.to_buf();
        let back = Report::from_buf(&buf);
        assert_eq!(back.status, r.status);
        assert_eq!(back.transaction_id, r.transaction_id);
        assert_eq!(back.data_size, r.data_size);
        assert_eq!(back.class, r.class);
        assert_eq!(back.id, r.id);
        assert_eq!(back.args, r.args);
        assert_eq!(back.status(), Status::Success);
    }

    /// The CRC span is documented as buffer bytes `[3..=88]` — which excludes `status` (buf[1])
    /// and `transaction_id` (buf[2]). Pins that exclusion (corrupting either must NOT desync a
    /// fresh recompute from the stored checksum) and that a byte genuinely inside the span
    /// (`class`, buf[7]) IS detectable as corruption — the actual mechanism a receiver would use
    /// to notice a mangled frame.
    #[test]
    fn crc_span_excludes_status_and_transaction_id_but_detects_class_corruption() {
        let r = Report::command(0x1F, 0x04, 0x85, 0x02);
        let buf = r.to_buf();
        let stored_crc = buf[89];

        let mut status_corrupt = buf;
        status_corrupt[1] ^= 0xFF;
        assert_eq!(
            crc(&status_corrupt),
            stored_crc,
            "status is outside the checksum span"
        );

        let mut tx_corrupt = buf;
        tx_corrupt[2] ^= 0xFF;
        assert_eq!(
            crc(&tx_corrupt),
            stored_crc,
            "transaction_id is outside the checksum span"
        );

        let mut class_corrupt = buf;
        class_corrupt[7] ^= 0xFF;
        assert_ne!(
            crc(&class_corrupt),
            stored_crc,
            "a corrupted class byte must be detectable against the stored checksum"
        );
    }

    /// `Status::from_u8` must map every documented code (New..Unsupported = 0..5) exactly, and
    /// fall back to `Other(v)` for anything outside that table — including the boundary value
    /// 255 — rather than panicking or silently aliasing to a known status.
    #[test]
    fn status_from_u8_maps_every_known_code_and_falls_back_to_other() {
        assert_eq!(Status::from_u8(0), Status::New);
        assert_eq!(Status::from_u8(1), Status::Busy);
        assert_eq!(Status::from_u8(2), Status::Success);
        assert_eq!(Status::from_u8(3), Status::Fail);
        assert_eq!(Status::from_u8(4), Status::Timeout);
        assert_eq!(Status::from_u8(5), Status::Unsupported);
        assert_eq!(Status::from_u8(6), Status::Other(6));
        assert_eq!(Status::from_u8(255), Status::Other(255));
    }

    /// An all-zero buffer (the boundary/degenerate input — e.g. a device that answers with a
    /// blank feature report) must decode sanely via `from_buf`: never panic/overflow-index, and
    /// every field reads back as zero (status 0 == `Status::New`).
    #[test]
    fn from_buf_on_all_zero_buffer_decodes_sanely_with_no_panic() {
        let buf = [0u8; BUF_LEN];
        let r = Report::from_buf(&buf);
        assert_eq!(r.status, 0);
        assert_eq!(r.transaction_id, 0);
        assert_eq!(r.data_size, 0);
        assert_eq!(r.class, 0);
        assert_eq!(r.id, 0);
        assert_eq!(r.args, [0u8; 80]);
        assert_eq!(r.status(), Status::New);
    }

    // ---- Property tests. `Report`'s fields are all raw u8/[u8;80] — the whole space is exactly
    // `any::<u8>()`/`any::<[u8;80]>()`, no domain narrowing needed.
    mod props {
        use super::*;
        use proptest::prelude::*;

        fn cfg() -> ProptestConfig {
            ProptestConfig { cases: 256, ..ProptestConfig::default() }
        }

        /// `[u8; 80]` generator — proptest's `array::uniformN` helpers stop at 32, so build the
        /// 80-byte args payload from a fixed-length `Vec` and convert (the length is pinned by
        /// `collection::vec`'s exact-size range, so `try_into` can never fail).
        fn args80() -> impl Strategy<Value = [u8; 80]> {
            proptest::collection::vec(any::<u8>(), 80)
                .prop_map(|v| v.try_into().expect("vec length pinned to 80 by the strategy"))
        }

        proptest! {
            #![proptest_config(cfg())]

            /// (a) `to_buf` → `from_buf` round-trips every field losslessly for ARBITRARY field
            /// values, generalizing `roundtrip_preserves_status_tx_id_and_full_args_payload`'s
            /// single fixed sample to the whole u8/[u8;80] space.
            #[test]
            fn report_roundtrips_through_its_buffer(
                status in any::<u8>(),
                transaction_id in any::<u8>(),
                data_size in any::<u8>(),
                class in any::<u8>(),
                id in any::<u8>(),
                args in args80(),
            ) {
                let r = Report { status, transaction_id, data_size, class, id, args };
                let buf = r.to_buf();
                let back = Report::from_buf(&buf);
                prop_assert_eq!(back.status, r.status);
                prop_assert_eq!(back.transaction_id, r.transaction_id);
                prop_assert_eq!(back.data_size, r.data_size);
                prop_assert_eq!(back.class, r.class);
                prop_assert_eq!(back.id, r.id);
                prop_assert_eq!(back.args, r.args);
            }

            /// (b) The CRC is an XOR-fold over exactly buffer bytes `[3..=88]` (`crc`, protocol.rs
            /// line ~99: `buf[3..=88].iter().fold(0u8, |c, &b| c ^ b)`). XOR-fold is a true law:
            /// flipping ANY SINGLE byte within that span always changes the fold (XOR is its own
            /// inverse — `old ^ new != 0` whenever `old != new`, and folding a changed term always
            /// changes the total for an XOR accumulator). So for an arbitrary report and an
            /// arbitrary index inside [3, 88] and an arbitrary non-zero XOR delta, corrupting that
            /// byte must change the recomputed crc relative to the original stored one.
            #[test]
            fn crc_detects_any_single_byte_corruption_in_its_span(
                status in any::<u8>(),
                transaction_id in any::<u8>(),
                data_size in any::<u8>(),
                class in any::<u8>(),
                id in any::<u8>(),
                args in args80(),
                span_offset in 0usize..=85, // 3..=88 is 86 positions; offset indexes into that span
                delta in 1u8..=255,          // non-zero XOR delta guarantees an actual change
            ) {
                let r = Report { status, transaction_id, data_size, class, id, args };
                let buf = r.to_buf();
                let original_crc = crc(&buf);

                let idx = 3 + span_offset;
                let mut corrupted = buf;
                corrupted[idx] ^= delta;
                prop_assert_ne!(
                    crc(&corrupted),
                    original_crc,
                    "flipping byte {} (inside the [3..=88] CRC span) with delta {:#04x} must change the crc",
                    idx, delta
                );
            }

            /// (b, complement) A byte OUTSIDE the CRC span ([0..=2] report-id/status/transaction_id,
            /// or [89..=90] the crc slot itself + reserved) never changes the recompute — pins the
            /// span boundary from the other side, so a future off-by-one in `crc`'s range literal
            /// trips one of these two properties.
            #[test]
            fn crc_ignores_corruption_outside_its_span(
                status in any::<u8>(),
                transaction_id in any::<u8>(),
                data_size in any::<u8>(),
                class in any::<u8>(),
                id in any::<u8>(),
                args in args80(),
                outside_idx in prop_oneof![0usize..=2, 89usize..=90],
                delta in 1u8..=255,
            ) {
                let r = Report { status, transaction_id, data_size, class, id, args };
                let buf = r.to_buf();
                let original_crc = crc(&buf);

                let mut corrupted = buf;
                corrupted[outside_idx] ^= delta;
                prop_assert_eq!(
                    crc(&corrupted),
                    original_crc,
                    "byte {} sits outside the [3..=88] CRC span and must not affect the recompute",
                    outside_idx
                );
            }

            /// (c) `reply_status` never panics for ANY slice length 0..96 and ANY content — extends
            /// the existing truncated-slice unit tests (length 8, length 0) to the whole short-slice
            /// space, and pins the documented `b.len() <= 8` guard: any buffer at or below that
            /// length always yields `None`.
            #[test]
            fn reply_status_never_panics_on_arbitrary_slices(
                buf in proptest::collection::vec(any::<u8>(), 0..96),
                class in any::<u8>(),
                id in any::<u8>(),
            ) {
                let result = reply_status(&buf, class, id); // must not panic
                if buf.len() <= 8 {
                    prop_assert_eq!(result, None, "reply_status must be None at/below its 8-byte guard");
                }
            }
        }
    }
}
