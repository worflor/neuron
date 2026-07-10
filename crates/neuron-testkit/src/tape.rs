//! ChromaTape — a timestamped recording of the native Chroma shared-memory sections while a
//! real game paints, and the decoder that replays it into tests.
//!
//! ## Provenance
//!
//! A tape is captured by a read-only sampler: every known `Global\{GUID}` section of the
//! Chroma SHM server is mapped `FILE_MAP_READ` and polled (~15 Hz); whenever a section's
//! content hash changes, one record is appended. Nothing is written to the sections and no
//! events are consumed, so a capture is invisible to both the game and the server. The
//! reference tape (`testdata/tapes/overwatch-combat.tape`) is ~35 s of live Overwatch play:
//! steady combat at the game's real cadence plus one scene transition, and one golden
//! snapshot of each bookkeeping section (session table, app registry, control roster).
//!
//! Because the game paints at ~10 fps and the sampler polls at ~15 Hz, the tape holds
//! essentially every distinct frame; what a poll-sampler cannot promise is sub-poll WRITE
//! ORDER between sections that changed in the same tick — treat same-tick records as one
//! logical frame fanned out, which is also what the lockstep record counts show.
//!
//! ## Format (little-endian, records back to back, no file header)
//!
//! ```text
//! [36 B] section GUID, ASCII, space-padded
//! [12 B] section nickname, ASCII, space-padded (e.g. "kbd-buffer")
//! [ 8 B] u64  t_ms — capture time, milliseconds since recording start
//! [ 4 B] u32  raw_len — decompressed section size in bytes
//! [ 4 B] u32  gz_len — length of the gzip payload that follows
//! [gz_len B] gzip-compressed full section snapshot (raw_len bytes when inflated)
//! ```
//!
//! ## Measured truths of the reference tape (pinned by tests)
//!
//! * Steady frame cadence ≈ 10 fps: keyboard-buffer inter-record p50 ≈ 96 ms (87–149 ms).
//! * One logical frame fans out to THREE buffers in lockstep: `kbd-buffer` (device-type
//!   0x01), `dev-type80`, and `big-stream` carry identical record counts.
//! * A device buffer is `[header .. 0x50) [pixel array]`, 4 bytes per pixel; a combat
//!   keyboard frame lights ~720 pixels in a handful of distinct colors. Byte 3 of a pixel
//!   is a per-key flag the games set (0xfd/0x02 observed) — replay it verbatim, don't
//!   interpret it.

use std::collections::BTreeMap;
use std::io::Read as _;
use std::path::Path;

use anyhow::{bail, Context, Result};

/// Offset of the pixel array inside a per-device buffer (header ends here).
pub const PIXEL_ARRAY_OFFSET: usize = 0x50;
/// Bytes per pixel in a device buffer's array.
pub const PIXEL_STRIDE: usize = 4;
/// Pixels in the KEYBOARD buffer's array — measured: the per-frame changing region of the
/// reference tape is exactly 0x50..0xB90 = 2880 bytes = 720 slots. Structures beyond it in
/// the same section change on other cadences and are NOT frame pixels.
pub const KBD_PIXEL_COUNT: usize = 720;

/// One sampled snapshot of one SHM section.
#[derive(Clone)]
pub struct Record {
    /// The section's `Global\{GUID}` name (braces and prefix stripped).
    pub guid: String,
    /// The recorder's nickname for the section (e.g. `kbd-buffer`, `session-table`).
    pub note: String,
    /// Milliseconds since recording start when this snapshot was captured.
    pub t_ms: u64,
    /// The full decompressed section content at that moment.
    pub bytes: Vec<u8>,
}

impl std::fmt::Debug for Record {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Record")
            .field("note", &self.note)
            .field("t_ms", &self.t_ms)
            .field("len", &self.bytes.len())
            .finish()
    }
}

/// A fully-decoded tape: every record, in capture order.
pub struct Tape {
    pub records: Vec<Record>,
}

impl Tape {
    /// Decode a `.tape` file. Fails loudly on any structural inconsistency — a fixture
    /// that half-parses is worse than one that errors.
    pub fn load(path: &Path) -> Result<Tape> {
        let data = std::fs::read(path).with_context(|| format!("read {}", path.display()))?;
        Self::parse(&data)
    }

    pub fn parse(data: &[u8]) -> Result<Tape> {
        let mut records = Vec::new();
        let mut off = 0usize;
        while off < data.len() {
            if data.len() - off < 64 {
                bail!("truncated record header at offset {off}");
            }
            let guid = std::str::from_utf8(&data[off..off + 36])
                .context("guid not ascii")?
                .trim()
                .to_string();
            let note = std::str::from_utf8(&data[off + 36..off + 48])
                .context("note not ascii")?
                .trim()
                .to_string();
            let t_ms = u64::from_le_bytes(data[off + 48..off + 56].try_into().unwrap());
            let raw_len = u32::from_le_bytes(data[off + 56..off + 60].try_into().unwrap()) as usize;
            let gz_len = u32::from_le_bytes(data[off + 60..off + 64].try_into().unwrap()) as usize;
            off += 64;
            if data.len() - off < gz_len {
                bail!("truncated gz payload for '{note}' at offset {off}");
            }
            let mut bytes = Vec::with_capacity(raw_len);
            flate2::read::GzDecoder::new(&data[off..off + gz_len])
                .read_to_end(&mut bytes)
                .with_context(|| format!("inflate '{note}' record at t={t_ms}ms"))?;
            if bytes.len() != raw_len {
                bail!(
                    "record '{note}' t={t_ms}: inflated {} bytes, header says {raw_len}",
                    bytes.len()
                );
            }
            off += gz_len;
            records.push(Record { guid, note, t_ms, bytes });
        }
        Ok(Tape { records })
    }

    /// Records of one section, in capture order.
    pub fn section(&self, note: &str) -> Vec<&Record> {
        self.records.iter().filter(|r| r.note == note).collect()
    }

    /// Record count per section nickname.
    pub fn census(&self) -> BTreeMap<String, usize> {
        let mut m = BTreeMap::new();
        for r in &self.records {
            *m.entry(r.note.clone()).or_insert(0usize) += 1;
        }
        m
    }

    /// Inter-record intervals (ms) for a section, transitions included.
    pub fn intervals_ms(&self, note: &str) -> Vec<u64> {
        let recs = self.section(note);
        recs.windows(2).map(|w| w[1].t_ms - w[0].t_ms).collect()
    }

    /// Median of the STEADY cadence for a section — intervals under `cut_ms` (scene
    /// transitions excluded). None when fewer than two steady intervals exist.
    pub fn steady_cadence_p50(&self, note: &str, cut_ms: u64) -> Option<u64> {
        let mut steady: Vec<u64> = self
            .intervals_ms(note)
            .into_iter()
            .filter(|&v| v < cut_ms)
            .collect();
        if steady.len() < 2 {
            return None;
        }
        steady.sort_unstable();
        Some(steady[steady.len() / 2])
    }
}

/// A device-buffer snapshot viewed as its pixel array (bounded — the section holds other
/// structures past the array that are not frame pixels).
pub struct PixelView<'a> {
    bytes: &'a [u8],
    count: usize,
}

impl<'a> PixelView<'a> {
    /// View a KEYBOARD buffer record's pixel array.
    pub fn of(record: &'a Record) -> PixelView<'a> {
        PixelView { bytes: &record.bytes, count: KBD_PIXEL_COUNT }
    }

    /// View with an explicit pixel count (other device-type buffers have smaller arrays).
    pub fn with_count(record: &'a Record, count: usize) -> PixelView<'a> {
        PixelView { bytes: &record.bytes, count }
    }

    /// Iterate `(offset, [b0,b1,b2,b3])` pixels from the array region.
    pub fn pixels(&self) -> impl Iterator<Item = (usize, [u8; 4])> + 'a {
        let start = PIXEL_ARRAY_OFFSET.min(self.bytes.len());
        let end = (PIXEL_ARRAY_OFFSET + self.count * PIXEL_STRIDE).min(self.bytes.len());
        self.bytes[start..end]
            .chunks_exact(PIXEL_STRIDE)
            .enumerate()
            .map(|(i, c)| (PIXEL_ARRAY_OFFSET + i * PIXEL_STRIDE, [c[0], c[1], c[2], c[3]]))
    }

    /// Pixels whose color bytes (b0..b2) are non-zero — the lit portion of the frame.
    pub fn lit(&self) -> Vec<(usize, [u8; 4])> {
        self.pixels()
            .filter(|(_, p)| p[0] != 0 || p[1] != 0 || p[2] != 0)
            .collect()
    }

    /// Distinct lit colors (b0..b2 triples).
    pub fn palette(&self) -> Vec<[u8; 3]> {
        let mut set = std::collections::BTreeSet::new();
        for (_, p) in self.lit() {
            set.insert([p[0], p[1], p[2]]);
        }
        set.into_iter().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn fixture() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../testdata/tapes/overwatch-combat.tape")
    }

    fn tape() -> Tape {
        Tape::load(&fixture()).expect("reference tape decodes")
    }

    #[test]
    fn reference_tape_decodes_completely() {
        let t = tape();
        assert!(
            t.records.len() > 900,
            "expected the full combat window, got {} records",
            t.records.len()
        );
    }

    #[test]
    fn one_frame_fans_out_to_three_buffers_in_lockstep() {
        let t = tape();
        let c = t.census();
        let kbd = c.get("kbd-buffer").copied().unwrap_or(0);
        assert!(kbd > 200, "combat window should hold hundreds of frames, got {kbd}");
        assert_eq!(c.get("dev-type80").copied(), Some(kbd), "type-0x80 buffer not in lockstep");
        assert_eq!(c.get("big-stream").copied(), Some(kbd), "stream buffer not in lockstep");
    }

    #[test]
    fn overwatch_paints_at_about_ten_fps() {
        let t = tape();
        let p50 = t.steady_cadence_p50("kbd-buffer", 1000).expect("steady cadence");
        assert!(
            (80..=130).contains(&p50),
            "reference cadence drifted: p50 = {p50} ms (recorded reality ≈ 96 ms)"
        );
    }

    #[test]
    fn bookkeeping_sections_have_exactly_one_golden_snapshot() {
        let t = tape();
        let c = t.census();
        for note in ["session-tabl", "session-info", "app-registry", "control-rost"] {
            assert_eq!(c.get(note).copied(), Some(1), "golden count for {note}");
        }
    }

    #[test]
    fn a_combat_keyboard_frame_has_the_measured_shape() {
        let t = tape();
        let frames = t.section("kbd-buffer");
        let mid = frames[frames.len() / 2];
        let view = PixelView::of(mid);
        let lit = view.lit();
        // Recorded reality: a combat frame lights the whole 720-slot array; fail if the
        // array offset/stride/count ever drifts (collapses lit to ~0 or misreads junk).
        assert!(
            (100..=KBD_PIXEL_COUNT).contains(&lit.len()),
            "lit pixel count {} outside the measured envelope (array = {KBD_PIXEL_COUNT})",
            lit.len()
        );
        let palette = view.palette();
        assert!(
            palette.len() >= 2,
            "a combat frame paints multiple colors, got {palette:?}"
        );
        // byte 3 is a per-key flag, not color — the observed vocabulary is tiny.
        let flags: std::collections::BTreeSet<u8> = lit.iter().map(|(_, p)| p[3]).collect();
        assert!(
            flags.len() <= 8,
            "pixel byte-3 should be a small flag vocabulary, saw {flags:?}"
        );
    }

    #[test]
    fn corrupt_tapes_error_instead_of_half_parsing() {
        let data = std::fs::read(fixture()).unwrap();
        // truncate mid-record
        let cut = &data[..data.len() / 2 + 13];
        assert!(Tape::parse(cut).is_err(), "truncated tape must not parse cleanly");
    }
}
