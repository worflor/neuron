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
/// hand-rolled. One frame vocabulary; the loops keep their own PACING (see DIALECT-RND.md:
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
}
