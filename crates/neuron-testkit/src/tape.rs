// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

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

/// Hard cap on one record's decompressed size. `raw_len` is an untrusted u32 read straight
/// from the file header (attacker/corruption-controlled), so it must never size an
/// allocation directly — a bogus header could otherwise demand ~4 GiB. Real SHM sections are
/// tiny (the biggest observed buffer is a few KiB); 16 MiB is a deliberately loose ceiling
/// that leaves generous headroom for future section types while still catching a hostile or
/// corrupted header before it can inflate unbounded.
const MAX_RECORD_BYTES: usize = 16 * 1024 * 1024;

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
            // Never size the allocation from `raw_len` — it's untrusted. Decode through a
            // bounded reader instead, so a hostile/corrupt header can only ever cost us
            // MAX_RECORD_BYTES of work, not an OOM-scale allocation up front.
            let mut bytes = Vec::new();
            let mut bounded =
                flate2::read::GzDecoder::new(&data[off..off + gz_len]).take(MAX_RECORD_BYTES as u64 + 1);
            bounded
                .read_to_end(&mut bytes)
                .with_context(|| format!("inflate '{note}' record at t={t_ms}ms"))?;
            if bytes.len() > MAX_RECORD_BYTES {
                bail!(
                    "record '{note}' t={t_ms}: inflated past the {MAX_RECORD_BYTES}-byte hard \
                     cap (corrupt or hostile raw_len={raw_len})"
                );
            }
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

    /// Build one raw `.tape` record: header fields plus a gzip payload made by compressing
    /// `payload`. `declared_raw_len` is written into the header as-is (may deliberately
    /// disagree with `payload.len()` to simulate a corrupt/hostile header).
    fn build_record(guid: &str, note: &str, t_ms: u64, declared_raw_len: u32, payload: &[u8]) -> Vec<u8> {
        use std::io::Write as _;
        let mut gz = Vec::new();
        {
            let mut enc = flate2::write::GzEncoder::new(&mut gz, flate2::Compression::default());
            enc.write_all(payload).unwrap();
            enc.finish().unwrap();
        }
        let mut out = Vec::new();
        out.extend_from_slice(format!("{guid:<36}").as_bytes());
        out.extend_from_slice(format!("{note:<12}").as_bytes());
        out.extend_from_slice(&t_ms.to_le_bytes());
        out.extend_from_slice(&declared_raw_len.to_le_bytes());
        out.extend_from_slice(&(gz.len() as u32).to_le_bytes());
        out.extend_from_slice(&gz);
        out
    }

    #[test]
    fn hostile_raw_len_errors_without_huge_allocation() {
        // raw_len claims ~4 GiB while the actual gzip payload is a couple bytes. Pre-fix this
        // called Vec::with_capacity(raw_len) before any validation; now raw_len never sizes
        // an allocation, so this must fail fast on the length mismatch instead of hanging or
        // aborting the process with an OOM.
        let data = build_record("hostile-guid", "hostile", 1, u32::MAX, b"hi");
        let err = Tape::parse(&data).err().expect("u32::MAX raw_len must not parse cleanly");
        assert!(
            err.to_string().contains("inflated"),
            "expected a length-mismatch error, got: {err}"
        );
    }

    #[test]
    fn declared_raw_len_smaller_than_actual_inflate_errors() {
        // The gzip stream honestly inflates to more bytes than the header declares — a
        // corruption signal that must be a loud error, not a silent truncation.
        let payload = vec![7u8; 1000];
        let data = build_record("mismatch-guid", "mismatch", 2, 10, &payload);
        let err = Tape::parse(&data)
            .err()
            .expect("oversized inflate vs. declared raw_len must error");
        assert!(
            err.to_string().contains("inflated"),
            "expected a length-mismatch error, got: {err}"
        );
    }

    #[test]
    fn inflate_past_hard_cap_errors() {
        // A record that honestly (declared == actual) inflates past MAX_RECORD_BYTES must
        // still be rejected — the cap is a hard ceiling, not just a mismatch check. Zeros
        // compress to almost nothing, so this stays a fast test despite the large logical
        // payload.
        let big_len = MAX_RECORD_BYTES + 1024;
        let payload = vec![0u8; big_len];
        let data = build_record("oversize-guid", "oversize", 3, big_len as u32, &payload);
        let err = Tape::parse(&data).err().expect("payload past the hard cap must error");
        assert!(
            err.to_string().contains("hard cap"),
            "expected the hard-cap error, got: {err}"
        );
    }

    #[test]
    fn well_formed_synthetic_record_still_parses() {
        // Sanity check that the bounded-read path doesn't break the happy case.
        let payload = b"a well formed section snapshot".to_vec();
        let data = build_record("ok-guid", "ok-note", 4, payload.len() as u32, &payload);
        let t = Tape::parse(&data).expect("well-formed record should parse");
        assert_eq!(t.records.len(), 1);
        assert_eq!(t.records[0].bytes, payload);
        assert_eq!(t.records[0].note, "ok-note");
    }

    // ---- Mutation-based property harness -----------------------------------------------

    use proptest::prelude::*;
    use std::sync::OnceLock;

    /// Reference tape bytes, read once and cached. House rule: never skip a fixture-backed
    /// test silently — if the recorded-reality corpus is missing, panic loudly so the gap is
    /// impossible to miss.
    fn real_tape_bytes() -> &'static [u8] {
        static BYTES: OnceLock<Vec<u8>> = OnceLock::new();
        BYTES.get_or_init(|| {
            std::fs::read(fixture()).unwrap_or_else(|e| {
                panic!(
                    "reference tape {} is required for mutation testing and could not be read: {e}",
                    fixture().display()
                )
            })
        })
    }

    /// Bound on how much of the real (multi-MB) tape a single mutation case clones. Slicing a
    /// bounded window keeps per-case cost tiny regardless of how large the fixture grows, while
    /// still mutating real, structurally valid bytes rather than synthetic ones.
    const MUTATION_WINDOW_CAP: usize = 64 * 1024;

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(256))]
        #[test]
        fn parse_never_panics_on_arbitrary_bytes(data in proptest::collection::vec(any::<u8>(), 0..8192)) {
            // Only requirement: no panic. `Err` is a perfectly fine outcome for garbage input.
            let _ = Tape::parse(&data);
        }
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(64))]
        #[test]
        fn parse_never_panics_on_mutated_real_tape(
            window_start_prop in 0.0f64..1.0,
            kind in 0u8..4u8,
            p1 in 0.0f64..1.0,
            p2 in 0.0f64..1.0,
            p3 in 0.0f64..1.0,
            byte_val in any::<u8>(),
            u32_val in any::<u32>(),
        ) {
            let real = real_tape_bytes();
            let real_len = real.len();
            let window_len = MUTATION_WINDOW_CAP.min(real_len);
            let max_start = real_len - window_len;
            let start = ((window_start_prop * max_start as f64) as usize).min(max_start);
            let mut window = real[start..start + window_len].to_vec();

            match kind {
                // (i) truncation at an arbitrary offset.
                0 => {
                    let cut = ((p1 * window_len as f64) as usize).min(window_len);
                    window.truncate(cut);
                }
                // (ii) single-byte corruption at an arbitrary position.
                1 => {
                    if window_len > 0 {
                        let pos = ((p1 * window_len as f64) as usize).min(window_len - 1);
                        window[pos] = byte_val;
                    }
                }
                // (iii) 4-byte little-endian overwrite — targets the length fields.
                2 => {
                    if window_len >= 4 {
                        let max_pos = window_len - 4;
                        let pos = ((p1 * max_pos as f64) as usize).min(max_pos);
                        window[pos..pos + 4].copy_from_slice(&u32_val.to_le_bytes());
                    }
                }
                // (iv) splice: swap two arbitrary equal-length windows.
                _ => {
                    let max_swap_len = window_len / 2;
                    if max_swap_len > 0 {
                        let swap_len = (((p3 * max_swap_len as f64) as usize).max(1)).min(max_swap_len);
                        let bound = window_len - swap_len;
                        let a = ((p1 * bound as f64) as usize).min(bound);
                        let b = ((p2 * bound as f64) as usize).min(bound);
                        if a != b {
                            let a_slice = window[a..a + swap_len].to_vec();
                            let b_slice = window[b..b + swap_len].to_vec();
                            window[a..a + swap_len].copy_from_slice(&b_slice);
                            window[b..b + swap_len].copy_from_slice(&a_slice);
                        }
                    }
                }
            }

            // The only hard requirement: never panic. `Err` is fine.
            let result = Tape::parse(&window);
            if let Ok(t) = result {
                // Structural sanity on anything that DID decode: the hard cap must hold, and
                // the query API must not panic when driven over whatever came out.
                for r in &t.records {
                    prop_assert!(r.bytes.len() <= MAX_RECORD_BYTES);
                }
                let census = t.census();
                for note in census.keys() {
                    let _ = t.section(note);
                    let _ = t.intervals_ms(note);
                }
            }
        }
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(256))]
        #[test]
        fn well_formed_multi_record_tapes_roundtrip(
            entries in proptest::collection::vec(
                (
                    proptest::sample::select(vec!["kbd-buffer", "dev-type80", "big-stream", "session-tabl", "misc-note"]),
                    1u64..500,
                    proptest::collection::vec(any::<u8>(), 0..300),
                ),
                1..20,
            )
        ) {
            const GUID: &str = "roundtrip-guid";

            // Strictly increasing global timestamps — matches how real tapes are captured and
            // keeps this test focused on roundtrip correctness rather than the (separate, real)
            // question of what happens when a section's timestamps go backwards.
            let mut running_t: u64 = 0;
            let mut recs: Vec<(String, u64, Vec<u8>)> = Vec::with_capacity(entries.len());
            for (note, delta, payload) in &entries {
                running_t += *delta;
                recs.push((note.to_string(), running_t, payload.clone()));
            }

            let mut data = Vec::new();
            for (note, t_ms, payload) in &recs {
                data.extend(build_record(GUID, note, *t_ms, payload.len() as u32, payload));
            }

            let tape = Tape::parse(&data).expect("well-formed synthetic multi-record tape must parse");
            prop_assert_eq!(tape.records.len(), recs.len());
            for (i, (note, t_ms, payload)) in recs.iter().enumerate() {
                prop_assert_eq!(&tape.records[i].note, note);
                prop_assert_eq!(tape.records[i].t_ms, *t_ms);
                prop_assert_eq!(&tape.records[i].bytes, payload);
            }

            // Pin section()/census()/intervals_ms() against the known construction.
            let mut by_note: BTreeMap<String, Vec<(u64, Vec<u8>)>> = BTreeMap::new();
            for (note, t_ms, payload) in &recs {
                by_note.entry(note.clone()).or_default().push((*t_ms, payload.clone()));
            }

            let census = tape.census();
            prop_assert_eq!(census.len(), by_note.len());
            for (note, expected) in &by_note {
                prop_assert_eq!(census.get(note).copied(), Some(expected.len()));

                let section = tape.section(note);
                prop_assert_eq!(section.len(), expected.len());
                for (r, (t_ms, payload)) in section.iter().zip(expected.iter()) {
                    prop_assert_eq!(r.t_ms, *t_ms);
                    prop_assert_eq!(&r.bytes, payload);
                }

                let intervals = tape.intervals_ms(note);
                let expected_intervals: Vec<u64> =
                    expected.windows(2).map(|w| w[1].0 - w[0].0).collect();
                prop_assert_eq!(intervals, expected_intervals);
            }
        }
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(256))]
        #[test]
        fn pixel_view_is_bounds_safe(
            len in 0usize..2048,
            count in 0usize..600,
            seed in any::<u8>(),
        ) {
            let bytes: Vec<u8> = (0..len).map(|i| seed.wrapping_add(i as u8)).collect();
            let record = Record { guid: String::new(), note: String::new(), t_ms: 0, bytes };

            // Mirror PixelView::pixels()'s own bounds math to pin the exact boundary.
            let start = PIXEL_ARRAY_OFFSET.min(len);
            let end = (PIXEL_ARRAY_OFFSET + count * PIXEL_STRIDE).min(len);
            let expected = (end - start) / PIXEL_STRIDE;

            let view = PixelView::with_count(&record, count);
            let pixels: Vec<_> = view.pixels().collect();
            prop_assert_eq!(pixels.len(), expected);
            for (off, _) in &pixels {
                prop_assert!(*off >= start && *off < end);
            }
            // Never yield more pixels than (len - offset) / stride, even when len < offset.
            let max_possible = len.saturating_sub(PIXEL_ARRAY_OFFSET) / PIXEL_STRIDE;
            prop_assert!(pixels.len() <= max_possible);

            let lit = view.lit();
            prop_assert!(lit.len() <= pixels.len());
            let palette = view.palette();
            prop_assert!(palette.len() <= lit.len());

            // Also exercise the fixed KEYBOARD-count path (PixelView::of) over the same
            // arbitrary, possibly-undersized, possibly-unaligned payload.
            let kbd_view = PixelView::of(&record);
            let kbd_pixels: Vec<_> = kbd_view.pixels().collect();
            let kbd_start = PIXEL_ARRAY_OFFSET.min(len);
            let kbd_end = (PIXEL_ARRAY_OFFSET + KBD_PIXEL_COUNT * PIXEL_STRIDE).min(len);
            prop_assert_eq!(kbd_pixels.len(), (kbd_end - kbd_start) / PIXEL_STRIDE);
        }
    }
}
