//! Portable clipboards — "pockets".
//!
//! A pocket is a named, single-payload clipboard register. Activating a `pocket` Action MOVES
//! content between the OS clipboard and the pocket, and the live clipboard decides the direction:
//!
//! ```text
//!   clipboard full,  pocket empty  ->  stash    (clipboard -> pocket, clipboard cleared)
//!   clipboard empty, pocket full   ->  restore  (pocket -> clipboard, pocket cleared)
//!   clipboard full,  pocket full   ->  swap     (exchange the two)
//!   both empty                     ->  nothing
//! ```
//!
//! The move is just an unconditional exchange `(clipboard, pocket) -> (pocket, clipboard)`; the
//! three named cases above are that same exchange with one side empty. Never loses data.
//!
//! FULL FIDELITY, on purpose: every clipboard format present (text, images via `CF_DIB`, files via
//! `CF_HDROP`, HTML/RTF, any registered format) is snapshotted and restored byte-for-byte — not
//! just text. The one thing it cannot carry is a handle-only format with no memory-backed twin
//! (e.g. a bare `CF_BITMAP` with no `CF_DIB`); rather than silently drop it, a move that would have
//! to displace such content REFUSES and leaves the clipboard exactly as it found it.
//!
//! `slot` is the identity: two bindings to the same slot share one register, different slots are
//! independent, and an empty name is the single default pocket. `persist` makes a pocket DURABLE —
//! its bytes are mirrored under `runtime/pockets/` so it survives a restart. That portability has a
//! cost worth stating plainly: a durable pocket's contents (images and files included) are written
//! to disk in the clear. Ephemeral pockets (the default) never touch the disk.
//!
//! Writing the clipboard mutates user state, so a move honors the process arm gate: in SAFE /
//! disarmed mode it touches nothing and reports `[disarmed]`. Reading to render a preview is always
//! safe and never gated.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

// Standard clipboard format ids (stable Win32 constants; spelled out so this file needs no extra
// windows-sys feature imports for the platform-neutral view logic).
const CF_TEXT: u32 = 1;
const CF_DIB: u32 = 8;
const CF_UNICODETEXT: u32 = 13;
const CF_HDROP: u32 = 15;
const CF_DIBV5: u32 = 17;

/// One clipboard format's raw bytes, captured verbatim. `id` is the Win32 clipboard-format id
/// (a standard `CF_*` constant, or a registered format id `>= 0xC000`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ClipFormat {
    pub id: u32,
    pub bytes: Vec<u8>,
}

/// Everything currently held — the full multi-format clipboard snapshot, or empty.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Pocket {
    pub formats: Vec<ClipFormat>,
}

impl Pocket {
    pub fn empty() -> Self {
        Self::default()
    }
    pub fn is_empty(&self) -> bool {
        self.formats.is_empty()
    }
    fn get(&self, id: u32) -> Option<&[u8]> {
        self.formats
            .iter()
            .find(|f| f.id == id)
            .map(|f| f.bytes.as_slice())
    }

    /// A content-typed summary of what's inside, for the GUI to render a live representation of the
    /// data rather than a static label. Pure (no OS calls): derived from the captured bytes.
    pub fn view(&self) -> PocketView {
        if self.is_empty() {
            return PocketView::default();
        }
        let n = self.formats.len();
        // Text first — the common case, and the most useful preview.
        if let Some(b) = self.get(CF_UNICODETEXT) {
            let s = utf16_to_string(b);
            let chars = s.chars().count();
            return PocketView {
                kind: PocketKind::Text,
                summary: format!("{chars} char{}", plural(chars)),
                text: Some(snippet(&s)),
                format_count: n,
                ..Default::default()
            };
        }
        if let Some(b) = self.get(CF_TEXT) {
            let s = String::from_utf8_lossy(b);
            let s = s.trim_end_matches('\0');
            let chars = s.chars().count();
            return PocketView {
                kind: PocketKind::Text,
                summary: format!("{chars} char{}", plural(chars)),
                text: Some(snippet(s)),
                format_count: n,
                ..Default::default()
            };
        }
        // Files (a CF_HDROP drop list).
        if let Some(b) = self.get(CF_HDROP) {
            let files = parse_hdrop(b);
            let count = files.len();
            return PocketView {
                kind: PocketKind::Files,
                summary: format!("{count} file{}", plural(count)),
                files: files.iter().map(|f| basename(f)).collect(),
                format_count: n,
                ..Default::default()
            };
        }
        // Image (a device-independent bitmap). CF_DIBV5 preferred, then CF_DIB; CF_BITMAP is
        // handle-only and unreachable here (it has no bytes), but the system almost always
        // synthesizes a CF_DIB twin, which is what we carry.
        if let Some(b) = self.get(CF_DIBV5).or_else(|| self.get(CF_DIB)) {
            let dims = dib_dims(b);
            let summary = match dims {
                Some((w, h)) => format!("{w}\u{00d7}{h} image"),
                None => "image".into(),
            };
            return PocketView {
                kind: PocketKind::Image,
                summary,
                image: dims,
                format_count: n,
                ..Default::default()
            };
        }
        // Something else (HTML-only, RTF-only, a custom app format…). Honest about it.
        PocketView {
            kind: PocketKind::Other,
            summary: format!("{n} format{}", plural(n)),
            format_count: n,
            ..Default::default()
        }
    }

    /// A deterministic, content-derived **sigil** for whatever this pocket holds — the same
    /// eigenmotion ink the spellweaving sigils are drawn with, but the oscillator spectrum is
    /// seeded from the payload's bytes instead of the rhythm brain. So the same content always
    /// draws the same mark, different content looks visibly different, and the *kind* biases the
    /// structure (text / files / images each read as a family) so a glance "quickly communicates
    /// it" while the exact shape is unique to those exact bytes. Pairs with [`PocketView::summary`]
    /// for the legible half. Bundles both via [`Pocket::sigil`].
    pub fn sigil(&self, samples: usize) -> Sigil {
        let v = self.view();
        Sigil {
            path: self.sigil_path(samples),
            summary: v.summary,
            kind: v.kind,
        }
    }

    /// The raw sigil stroke: a normalized `[-1,1]²` path from a bank of damped complex oscillators
    /// `z[n] = K·z[n-1]` whose eigenvalues `K` are derived from the payload's bytes + kind. Same
    /// integration as [`crate::twin::Familiar::sigil_path`]; the *spectrum* is the data's, not the
    /// brain's. Empty pocket → empty path. (Hashes the whole payload, so call it on change, not
    /// per-frame.)
    pub fn sigil_path(&self, samples: usize) -> Vec<(f32, f32)> {
        if self.is_empty() || samples < 2 {
            return Vec::new();
        }
        let kind = self.view().kind;
        // Seed from the kind (family) + every format's id and bytes (the unique variation).
        let mut seed = 0xcbf2_9ce4_8422_2325u64 ^ (kind as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15);
        for f in &self.formats {
            seed = mix(seed, &f.id.to_le_bytes());
            seed = mix(seed, &f.bytes);
        }
        // Kind sets the oscillator count → a recognizable structural family; bytes set the rest.
        let banks = (match kind {
            PocketKind::Text => 4,
            PocketKind::Files => 5,
            PocketKind::Image => 6,
            _ => 5,
        } + usize::from(self.formats.len() > 2))
        .max(2);

        let mut rng = SplitMix64(seed);
        let mut ks: Vec<(f64, f64)> = Vec::with_capacity(banks);
        let mut state: Vec<(f64, f64)> = Vec::with_capacity(banks);
        for b in 0..banks {
            // magnitude just inside the unit circle → a bounded rosette (rises slightly per bank)
            let mag = 0.95 + 0.04 * (b as f64 / banks as f64);
            // a small integer harmonic from the hash → petals/knots; signed for handedness
            let freq = 1.0 + (rng.next() % 5) as f64;
            let dir = if rng.next() & 1 == 0 { 1.0 } else { -1.0 };
            let omega = dir * std::f64::consts::TAU * freq / samples as f64;
            let phase = rng.unit() * std::f64::consts::TAU;
            ks.push((mag * omega.cos(), mag * omega.sin()));
            state.push((phase.cos(), phase.sin()));
        }
        let mut raw = Vec::with_capacity(samples);
        let mut maxr = 1e-6f64;
        for _ in 0..samples {
            let (mut x, mut y) = (0.0f64, 0.0f64);
            for b in 0..banks {
                let (zr, zi) = state[b];
                let (kr, ki) = ks[b];
                state[b] = (kr * zr - ki * zi, kr * zi + ki * zr);
                let w = 1.0 / (b as f64 + 1.0); // taper the higher banks
                x += state[b].0 * w;
                y += state[b].1 * w;
            }
            maxr = maxr.max(x.hypot(y));
            raw.push((x, y));
        }
        raw.into_iter()
            .map(|(x, y)| ((x / maxr) as f32, (y / maxr) as f32))
            .collect()
    }

    /// The sigil as a standalone hard-light SVG — reuses the spellweaving sigil renderer so a
    /// pocket's fingerprint can be SEEN headlessly (`neuron pocket <name> --sigil out.svg`).
    pub fn sigil_svg(&self, size: f32) -> String {
        crate::scene::sigil_scene(&self.sigil_path(256), size).to_svg()
    }
}

/// A pocket's renderable identity: the emergent stroke + the legible summary + the kind (for the
/// accent/icon). The GUI draws the `path` in the pocket's accent and shows `summary` beside it.
#[derive(Clone, Default)]
pub struct Sigil {
    pub path: Vec<(f32, f32)>,
    pub summary: String,
    pub kind: PocketKind,
}

/// FNV-1a step over a byte run (folds a payload into the sigil seed).
fn mix(mut h: u64, bytes: &[u8]) -> u64 {
    for &b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(0x100_0000_01b3);
    }
    h
}

/// SplitMix64 — a tiny deterministic PRNG for the sigil spectrum (no `rand` dep, matching the
/// project's lean ethos and the effects engine's hand-rolled generators).
struct SplitMix64(u64);

impl SplitMix64 {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    fn unit(&mut self) -> f64 {
        (self.next() >> 11) as f64 / (1u64 << 53) as f64
    }
}

/// What a pocket is holding, at a glance — drives the icon/representation in the UI.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PocketKind {
    Empty,
    Text,
    Files,
    Image,
    Other,
}

impl Default for PocketKind {
    fn default() -> Self {
        PocketKind::Empty
    }
}

/// A renderable view of a pocket's payload: enough for the GUI to *show the data*, not just name it.
#[derive(Clone, Default)]
pub struct PocketView {
    pub kind: PocketKind,
    /// One-line summary: "240 chars" / "3 files" / "1920×1080 image" / "2 formats" / "empty".
    pub summary: String,
    /// A short text preview (first line, capped) when the payload is text.
    pub text: Option<String>,
    /// File basenames when the payload is a file drop.
    pub files: Vec<String>,
    /// Pixel dimensions when the payload is an image.
    pub image: Option<(u32, u32)>,
    /// How many distinct clipboard formats are stored (the fidelity count).
    pub format_count: usize,
}

impl PocketView {
    pub fn is_empty(&self) -> bool {
        self.kind == PocketKind::Empty
    }
}

// ---- the store: one process-global map of slot -> contents -----------------------------------

#[derive(Default)]
struct Slot {
    pocket: Pocket,
    durable: bool,
}

static SLOTS: OnceLock<Mutex<HashMap<String, Slot>>> = OnceLock::new();

fn slots() -> &'static Mutex<HashMap<String, Slot>> {
    SLOTS.get_or_init(|| Mutex::new(load_all()))
}

/// Bumped on every successful move. The GUI watches this so it rebuilds its pocket representation
/// (which re-hashes payloads to draw sigils) ONLY when something actually changed — never per-frame.
static GEN: AtomicU64 = AtomicU64::new(0);

/// The change counter — increments each time a pocket's contents move. See [`GEN`].
pub fn generation() -> u64 {
    GEN.load(Ordering::Relaxed)
}

/// Activate a pocket: move content between the OS clipboard and this slot (see the module doc for
/// the truth table). `persist` marks the slot durable (mirrored to disk, survives a restart).
/// Returns a short status line describing what moved. The only entry point the `Action` layer uses.
pub fn activate(slot: &str, persist: bool) -> String {
    let tag = if slot.is_empty() {
        String::new()
    } else {
        format!(" {slot}")
    };

    let (live, live_empty) = match read_clip_state() {
        // The clipboard holds something we can't snapshot (a handle-only format with no DIB/HGLOBAL
        // twin). Moving would either lose it (on a stash) or clobber it (on a restore), so we refuse
        // and change nothing — the same "never destroy what you didn't ask to" rule the device
        // writes follow.
        ClipState::Uncarryable => {
            return format!("pocket{tag}: clipboard holds content i can't carry \u{2014} left it");
        }
        ClipState::Empty => (Pocket::empty(), true),
        ClipState::Carryable(p) => (p, false),
    };

    // Poison-recoverable BECAUSE the exchange below is panic-safe by construction: the clipboard
    // write happens FIRST, borrowing the slot in place, and only then does a single-step
    // `mem::replace` swap the slot's content — so a panic anywhere in this critical section leaves
    // the slot in a VALID state (pre-exchange, payload intact) rather than a torn "silently
    // emptied" one. That ordering is what makes `into_inner` safe here; do not reorder the
    // exchange back to take-then-write without restoring a bare unwrap and its rationale.
    let mut g = slots().lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let entry = g.entry(slot.to_string()).or_default();
    let stored_empty = entry.pocket.is_empty();

    if live_empty && stored_empty {
        return format!("pocket{tag}: nothing to move");
    }
    // The move writes the clipboard, which mutates user state — gate it like every other synthesis.
    if !crate::action::input_armed() {
        return format!("pocket{tag} [disarmed]");
    }

    // The whole semantics: an unconditional exchange. clipboard <- old pocket; pocket <- old live.
    // FAILURE-SAFETY ORDER (load-bearing — see the lock comment above): write the clipboard FIRST,
    // borrowing the slot in place, and swap ONLY once that write reports success — so BOTH failure
    // shapes leave the slot untouched with the user's payload intact: a panic inside
    // set_clipboard (unwinds before the swap), and the ordinary fallible path (a foreign process
    // holding the clipboard → `false` → honest "nothing moved", never a silently-consumed pocket).
    let new_pocket = live; // what was on the clipboard now rests in the pocket
    if !set_clipboard(&entry.pocket) {
        // what was pocketed COULD NOT reach the clipboard — moving it out anyway would destroy
        // it (the old clipboard content still sits on the clipboard AND would land in the slot).
        return format!("pocket{tag}: clipboard is held by another app \u{2014} nothing moved");
    }
    let to_clipboard = std::mem::replace(&mut entry.pocket, new_pocket);

    entry.durable |= persist;
    let durable = entry.durable;
    if durable {
        // Persist OFF the dispatch path: a multi-MB image pocket shouldn't block the live tick on a
        // synchronous file write. Snapshot under the lock, write on a worker.
        let slot = slot.to_string();
        let snapshot = entry.pocket.clone();
        crate::worker::spawn_detached("neuron-pocket-persist", move || {
            let _ = write_disk(&slot, &snapshot);
        });
    }
    GEN.fetch_add(1, Ordering::Relaxed); // a real move happened — let the GUI repaint

    // Word the result by which sides were full, and show *what* landed (its content view).
    match (live_empty, stored_empty) {
        (false, true) => format!(
            "pocket{tag} \u{2190} stashed ({})",
            entry.pocket.view().summary
        ),
        (true, false) => format!(
            "pocket{tag} \u{2192} clipboard ({})",
            to_clipboard.view().summary
        ),
        (false, false) => {
            format!(
                "pocket{tag} \u{21c4} swapped (now holds {})",
                entry.pocket.view().summary
            )
        }
        (true, true) => unreachable!("guarded above"),
    }
}

/// Every pocket's live contents, for the GUI to render: `(slot, durable, view)`. Read-only, safe.
pub fn views() -> Vec<(String, bool, PocketView)> {
    let g = slots().lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut out: Vec<_> = g
        .iter()
        .map(|(k, v)| (k.clone(), v.durable, v.pocket.view()))
        .collect();
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

/// One named pocket's live view (empty if it doesn't exist yet). Read-only, safe.
pub fn view_of(slot: &str) -> PocketView {
    slots()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(slot)
        .map(|s| s.pocket.view())
        .unwrap_or_default()
}

/// One named pocket's content-sigil (stroke + summary + kind), for the GUI to render. Read-only.
pub fn sigil_of(slot: &str, samples: usize) -> Sigil {
    slots()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(slot)
        .map(|s| s.pocket.sigil(samples))
        .unwrap_or_default()
}

/// One named pocket's sigil as an SVG (empty string if the pocket is empty/absent). Read-only.
pub fn sigil_svg_of(slot: &str, size: f32) -> String {
    slots()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(slot)
        .map(|s| s.pocket.sigil_svg(size))
        .unwrap_or_default()
}

// ---- preview helpers (platform-neutral, unit-tested) -----------------------------------------

fn plural(n: usize) -> &'static str {
    if n == 1 {
        ""
    } else {
        "s"
    }
}

fn utf16_to_string(b: &[u8]) -> String {
    let units: Vec<u16> = b
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .collect();
    let end = units.iter().position(|&u| u == 0).unwrap_or(units.len());
    String::from_utf16_lossy(&units[..end])
}

/// First non-empty line, whitespace-collapsed, capped — a glanceable preview, not the whole blob.
fn snippet(s: &str) -> String {
    let line = s
        .lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
        .trim();
    let collapsed: String = {
        let mut out = String::new();
        let mut ws = false;
        for ch in line.chars() {
            if ch.is_whitespace() {
                ws = true;
            } else {
                if ws && !out.is_empty() {
                    out.push(' ');
                }
                ws = false;
                out.push(ch);
            }
        }
        out
    };
    if collapsed.chars().count() > 80 {
        let cut: String = collapsed.chars().take(79).collect();
        format!("{cut}\u{2026}")
    } else {
        collapsed
    }
}

fn basename(path: &str) -> String {
    path.rsplit(['\\', '/']).next().unwrap_or(path).to_string()
}

/// Parse a `CF_HDROP` `DROPFILES` block into the dropped file paths (wide or ANSI, double-null
/// terminated list at the `pFiles` offset).
fn parse_hdrop(b: &[u8]) -> Vec<String> {
    if b.len() < 20 {
        return Vec::new();
    }
    let p_files = u32::from_le_bytes([b[0], b[1], b[2], b[3]]) as usize;
    let wide = u32::from_le_bytes([b[16], b[17], b[18], b[19]]) != 0;
    if p_files >= b.len() {
        return Vec::new();
    }
    let data = &b[p_files..];
    let mut out = Vec::new();
    if wide {
        let units: Vec<u16> = data
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        let mut start = 0;
        for i in 0..units.len() {
            if units[i] == 0 {
                if i == start {
                    break; // the double-null that ends the list
                }
                out.push(String::from_utf16_lossy(&units[start..i]));
                start = i + 1;
            }
        }
    } else {
        let mut start = 0;
        for i in 0..data.len() {
            if data[i] == 0 {
                if i == start {
                    break;
                }
                out.push(String::from_utf8_lossy(&data[start..i]).into_owned());
                start = i + 1;
            }
        }
    }
    out
}

/// Pull pixel dimensions out of a DIB's header (`BITMAPINFOHEADER` or `BITMAPV5HEADER` — both keep
/// width at offset 4 and height at offset 8). Height may be negative (top-down); we report the
/// magnitude.
fn dib_dims(b: &[u8]) -> Option<(u32, u32)> {
    if b.len() < 12 {
        return None;
    }
    let w = i32::from_le_bytes([b[4], b[5], b[6], b[7]]);
    let h = i32::from_le_bytes([b[8], b[9], b[10], b[11]]);
    Some((w.unsigned_abs(), h.unsigned_abs()))
}

/// Mirror EVERY durable slot to disk NOW, synchronously — the exit barrier for one-shot processes.
/// `activate()` persists on a worker thread (right for the resident app, which must not block the
/// live tick on a multi-MB file write), but a process that exits right after the move kills that
/// worker mid-write and the user's payload with it. A one-shot caller (the CLI) runs this before
/// returning. Idempotent full re-mirror: full slots written, emptied slots' files removed.
pub fn flush_durable_sync() {
    let snapshot: Vec<(String, Pocket)> = {
        let g = slots()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        g.iter()
            .filter(|(_, s)| s.durable)
            .map(|(k, s)| (k.clone(), s.pocket.clone()))
            .collect()
    };
    for (slot, p) in snapshot {
        let _ = write_disk(&slot, &p);
    }
}

// ---- durable-pocket persistence (binary, handles any payload) ---------------------------------

const MAGIC: &[u8; 4] = b"NPKT";
const VERSION: u8 = 1;

/// `runtime/pockets/` in the run root — where durable pockets are mirrored.
fn disk_dir() -> PathBuf {
    crate::runroot::run_root().join("runtime").join("pockets")
}

fn fnv1a(s: &str) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for &byte in s.as_bytes() {
        h ^= byte as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

fn disk_path(slot: &str) -> PathBuf {
    // The slot name can be anything (including empty / path-unsafe), so the filename is a hash and
    // the real name is stored inside the file.
    disk_dir().join(format!("{:016x}.pocket", fnv1a(slot)))
}

fn put_bytes(buf: &mut Vec<u8>, b: &[u8]) {
    buf.extend_from_slice(&(b.len() as u32).to_le_bytes());
    buf.extend_from_slice(b);
}

fn write_disk(slot: &str, p: &Pocket) -> std::io::Result<()> {
    let path = disk_path(slot);
    if p.is_empty() {
        // An emptied durable pocket leaves no file behind.
        let _ = std::fs::remove_file(&path);
        return Ok(());
    }
    let mut buf = Vec::new();
    buf.extend_from_slice(MAGIC);
    buf.push(VERSION);
    put_bytes(&mut buf, slot.as_bytes());
    buf.extend_from_slice(&(p.formats.len() as u32).to_le_bytes());
    for f in &p.formats {
        buf.extend_from_slice(&f.id.to_le_bytes());
        put_bytes(&mut buf, &f.bytes);
    }
    std::fs::create_dir_all(disk_dir())?;
    std::fs::write(path, buf)
}

fn take_bytes<'a>(b: &mut &'a [u8]) -> Option<&'a [u8]> {
    if b.len() < 4 {
        return None;
    }
    let len = u32::from_le_bytes([b[0], b[1], b[2], b[3]]) as usize;
    let rest = &b[4..];
    if rest.len() < len {
        return None;
    }
    let (out, tail) = rest.split_at(len);
    *b = tail;
    Some(out)
}

fn parse_disk(mut b: &[u8]) -> Option<(String, Pocket)> {
    if b.len() < 5 || &b[0..4] != MAGIC || b[4] != VERSION {
        return None;
    }
    b = &b[5..];
    let name = String::from_utf8_lossy(take_bytes(&mut b)?).into_owned();
    if b.len() < 4 {
        return None;
    }
    let count = u32::from_le_bytes([b[0], b[1], b[2], b[3]]) as usize;
    b = &b[4..];
    let mut formats = Vec::with_capacity(count);
    for _ in 0..count {
        if b.len() < 4 {
            return None;
        }
        let id = u32::from_le_bytes([b[0], b[1], b[2], b[3]]);
        b = &b[4..];
        let bytes = take_bytes(&mut b)?.to_vec();
        formats.push(ClipFormat { id, bytes });
    }
    Some((name, Pocket { formats }))
}

fn load_all() -> HashMap<String, Slot> {
    let mut map = HashMap::new();
    let Ok(rd) = std::fs::read_dir(disk_dir()) else {
        return map;
    };
    for entry in rd.flatten() {
        if entry.path().extension().and_then(|e| e.to_str()) != Some("pocket") {
            continue;
        }
        if let Ok(bytes) = std::fs::read(entry.path()) {
            if let Some((name, pocket)) = parse_disk(&bytes) {
                map.insert(
                    name,
                    Slot {
                        pocket,
                        durable: true,
                    },
                );
            }
        }
    }
    map
}

// ---- clipboard state, read non-destructively / written atomically -----------------------------

enum ClipState {
    Empty,
    /// Clipboard has content, but none of it is in a form we can snapshot (handle-only formats).
    Uncarryable,
    Carryable(Pocket),
}

// ---- test seam: an injectable in-memory clipboard (OFF by default) ----------------------------
//
// Production reads/writes the OS clipboard (the `imp` module below). To let the integration tests
// exercise the FULL `activate()` move path — stash / restore / swap / persist / refuse-uncarryable —
// WITHOUT ever touching (or mutating) the user's real clipboard, a test can install an in-memory
// clipboard here; `activate()`'s reads and writes then hit that cell instead of the OS. It is OFF
// unless a test explicitly installs it, so production behavior is byte-for-byte unchanged. This is a
// test seam, not public API (`#[doc(hidden)]`).

/// What the in-memory test clipboard holds (mirrors [`ClipState`] but owns its payload).
enum FakeClip {
    Empty,
    Uncarryable,
    Carryable(Pocket),
    /// Reads succeed (the payload is visible) but every WRITE is refused — the real-world race
    /// where a foreign process grabs the clipboard between our read and our write
    /// (`OpenClipboard` fails → `imp::set_clipboard` returns false). Exists to pin `activate`'s
    /// nothing-moved guarantee on the ordinary fallible path, not just the panic path.
    Refuses(Pocket),
}

static FAKE_CLIP: OnceLock<Mutex<Option<FakeClip>>> = OnceLock::new();

fn fake_clip() -> &'static Mutex<Option<FakeClip>> {
    FAKE_CLIP.get_or_init(|| Mutex::new(None))
}

/// Read the clipboard state — the in-memory test clipboard if one is installed, else the OS.
fn read_clip_state() -> ClipState {
    if let Some(fake) = fake_clip().lock().unwrap_or_else(std::sync::PoisonError::into_inner).as_ref() {
        return match fake {
            FakeClip::Empty => ClipState::Empty,
            FakeClip::Uncarryable => ClipState::Uncarryable,
            FakeClip::Carryable(p) => ClipState::Carryable(p.clone()),
            // reads see the content normally — only the WRITE half is refused.
            FakeClip::Refuses(p) => ClipState::Carryable(p.clone()),
        };
    }
    imp::read_clip_state()
}

/// Write the clipboard — the in-memory test clipboard if installed, else the OS.
fn set_clipboard(p: &Pocket) -> bool {
    let mut g = fake_clip().lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    match g.as_ref() {
        // the write-refusing state stays exactly as installed — like a foreign process that
        // still holds the clipboard open when our write arrives.
        Some(FakeClip::Refuses(_)) => return false,
        Some(_) => {
            *g = Some(if p.is_empty() {
                FakeClip::Empty
            } else {
                FakeClip::Carryable(p.clone())
            });
            return true;
        }
        None => {}
    }
    drop(g);
    imp::set_clipboard(p)
}

/// TEST SEAM — drive [`activate`] against an in-memory clipboard so the move path is provable
/// non-destructively (it never touches the real OS clipboard). OFF in production unless installed.
#[doc(hidden)]
pub mod testclip {
    use super::{fake_clip, load_all, slots, ClipFormat, FakeClip, Pocket, Slot};

    const CF_UNICODETEXT: u32 = 13;

    fn utf16le(s: &str) -> Vec<u8> {
        let mut b: Vec<u8> = s.encode_utf16().flat_map(|u| u.to_le_bytes()).collect();
        b.extend_from_slice(&[0, 0]); // NUL terminator
        b
    }

    /// A single-format CF_UNICODETEXT text pocket — for seeding the fake clipboard or a slot.
    pub fn text_pocket(s: &str) -> Pocket {
        Pocket {
            formats: vec![ClipFormat {
                id: CF_UNICODETEXT,
                bytes: utf16le(s),
            }],
        }
    }

    /// Install an EMPTY in-memory clipboard (routes `activate()` away from the OS).
    pub fn install_empty() {
        *fake_clip().lock().unwrap_or_else(std::sync::PoisonError::into_inner) = Some(FakeClip::Empty);
    }
    /// Install an in-memory clipboard holding `s` as text.
    pub fn install_text(s: &str) {
        *fake_clip().lock().unwrap_or_else(std::sync::PoisonError::into_inner) = Some(FakeClip::Carryable(text_pocket(s)));
    }
    /// Install an in-memory clipboard holding an arbitrary multi-format payload.
    pub fn install_pocket(p: Pocket) {
        *fake_clip().lock().unwrap_or_else(std::sync::PoisonError::into_inner) = Some(if p.is_empty() {
            FakeClip::Empty
        } else {
            FakeClip::Carryable(p)
        });
    }
    /// Install an in-memory clipboard that READS as holding `s` but REFUSES every write — the
    /// foreign-holder race (`OpenClipboard` fails at write time). For pinning `activate`'s
    /// nothing-moved guarantee on the ordinary fallible path.
    pub fn install_refusing_text(s: &str) {
        *fake_clip().lock().unwrap_or_else(std::sync::PoisonError::into_inner) =
            Some(FakeClip::Refuses(text_pocket(s)));
    }
    /// Install an in-memory clipboard whose content can't be carried (handle-only, no twin).
    pub fn install_uncarryable() {
        *fake_clip().lock().unwrap_or_else(std::sync::PoisonError::into_inner) = Some(FakeClip::Uncarryable);
    }
    /// What currently sits on the in-memory clipboard (None if empty / uncarryable / not installed).
    pub fn current() -> Option<Pocket> {
        match fake_clip().lock().unwrap_or_else(std::sync::PoisonError::into_inner).as_ref() {
            Some(FakeClip::Carryable(p)) => Some(p.clone()),
            _ => None,
        }
    }
    /// Uninstall the in-memory clipboard (restore OS-clipboard routing).
    pub fn uninstall() {
        *fake_clip().lock().unwrap_or_else(std::sync::PoisonError::into_inner) = None;
    }
    /// Clear the in-memory pocket store (the slot map) for test isolation.
    pub fn reset_store() {
        slots().lock().unwrap_or_else(std::sync::PoisonError::into_inner).clear();
    }
    /// Seed one slot's pocket directly into the store (durable flag set), bypassing `activate()`.
    pub fn seed_slot(slot: &str, pocket: Pocket, durable: bool) {
        slots()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(slot.to_string(), Slot { pocket, durable });
    }
    /// Re-read the durable pockets from disk into the store (re-runs `load_all`, for tests that
    /// write `.pocket` files directly and then want them loaded).
    pub fn reload_disk() {
        *slots().lock().unwrap_or_else(std::sync::PoisonError::into_inner) = load_all();
    }
}

#[cfg(windows)]
mod imp {
    use super::{ClipFormat, ClipState, Pocket};
    use std::ptr;
    use windows_sys::Win32::Foundation::GlobalFree;
    use windows_sys::Win32::System::DataExchange::{
        CloseClipboard, EmptyClipboard, EnumClipboardFormats, GetClipboardData, OpenClipboard,
        SetClipboardData,
    };
    use windows_sys::Win32::System::Memory::{
        GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock, GMEM_MOVEABLE,
    };

    /// Open the clipboard, RETRYING briefly — another process (a clipboard manager, RDP, a browser)
    /// can hold it for a few ms, and a one-shot failure would make us needlessly refuse the move (or
    /// fail a restore). Caller must `CloseClipboard` on success.
    unsafe fn open_clipboard_retry() -> bool {
        for _ in 0..10 {
            if OpenClipboard(ptr::null_mut()) != 0 {
                return true;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        false
    }

    /// Read the whole clipboard WITHOUT changing it. Distinguishes truly empty from "has content we
    /// can't carry" so the caller can refuse rather than lose data.
    pub fn read_clip_state() -> ClipState {
        // OS-auto-synthesized derivatives: when the canonical source is on the clipboard, Windows can
        // regenerate these on demand. Snapshotting them and re-publishing them as REAL entries on a
        // restore would SUPPRESS that synthesis (EmptyClipboard kills auto-synthesis) and can subtly
        // change what a consuming app sees — so we skip a derivative whenever its source is present
        // and let the OS re-derive it. (Handle-only twins like CF_BITMAP/CF_PALETTE are already
        // dropped by `snapshot_one`.)
        const CF_TEXT: u32 = 1;
        const CF_OEMTEXT: u32 = 7;
        const CF_UNICODETEXT: u32 = 13;
        const CF_LOCALE: u32 = 16;
        const CF_DIB: u32 = 8;
        const CF_DIBV5: u32 = 17;
        unsafe {
            if !open_clipboard_retry() {
                // Couldn't open it even after retrying — treat as uncarryable so we never clobber a
                // clipboard we couldn't inspect.
                return ClipState::Uncarryable;
            }
            // Pass 1: enumerate which formats are present (we need the whole set to decide skips).
            let mut order = Vec::new();
            let mut fmt = EnumClipboardFormats(0);
            while fmt != 0 {
                order.push(fmt);
                fmt = EnumClipboardFormats(fmt);
            }
            let saw_any = !order.is_empty();
            let present = |id: u32| order.contains(&id);
            let skip = |id: u32| -> bool {
                match id {
                    CF_TEXT => present(CF_UNICODETEXT),
                    CF_OEMTEXT | CF_LOCALE => present(CF_UNICODETEXT) || present(CF_TEXT),
                    CF_DIB => present(CF_DIBV5),
                    _ => false,
                }
            };
            // Pass 2: snapshot only the formats we keep, in clipboard order.
            let mut formats = Vec::new();
            for &id in &order {
                if skip(id) {
                    continue;
                }
                if let Some(bytes) = snapshot_one(id) {
                    formats.push(ClipFormat { id, bytes });
                }
            }
            CloseClipboard();
            if !saw_any {
                ClipState::Empty
            } else if formats.is_empty() {
                ClipState::Uncarryable
            } else {
                ClipState::Carryable(Pocket { formats })
            }
        }
    }

    /// Copy one format's bytes out of its global memory block. Returns None for handle-only formats
    /// (CF_BITMAP / CF_PALETTE / metafiles) that aren't `GlobalLock`-able.
    unsafe fn snapshot_one(fmt: u32) -> Option<Vec<u8>> {
        let h = GetClipboardData(fmt); // owned by the clipboard — do NOT free.
        if h.is_null() {
            return None;
        }
        let p = GlobalLock(h);
        if p.is_null() {
            return None;
        }
        let size = GlobalSize(h);
        let bytes = std::slice::from_raw_parts(p as *const u8, size).to_vec();
        GlobalUnlock(h);
        Some(bytes)
    }

    /// Replace the clipboard with exactly these formats (empties it first). An empty `Pocket` just
    /// clears the clipboard.
    pub fn set_clipboard(p: &Pocket) -> bool {
        unsafe {
            if !open_clipboard_retry() {
                return false;
            }
            EmptyClipboard();
            for f in &p.formats {
                let h = GlobalAlloc(GMEM_MOVEABLE, f.bytes.len());
                if h.is_null() {
                    continue;
                }
                let dst = GlobalLock(h);
                if dst.is_null() {
                    GlobalFree(h);
                    continue;
                }
                std::ptr::copy_nonoverlapping(f.bytes.as_ptr(), dst as *mut u8, f.bytes.len());
                GlobalUnlock(h);
                // On success the system OWNS the block; on failure we must free it.
                if SetClipboardData(f.id, h).is_null() {
                    GlobalFree(h);
                }
            }
            CloseClipboard();
            true
        }
    }
}

#[cfg(not(windows))]
mod imp {
    use super::{ClipState, Pocket};
    pub fn read_clip_state() -> ClipState {
        ClipState::Empty
    }
    pub fn set_clipboard(_p: &Pocket) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn utf16le(s: &str) -> Vec<u8> {
        let mut b: Vec<u8> = s.encode_utf16().flat_map(|u| u.to_le_bytes()).collect();
        b.extend_from_slice(&[0, 0]); // NUL terminator
        b
    }

    fn text_pocket(s: &str) -> Pocket {
        Pocket {
            formats: vec![ClipFormat {
                id: CF_UNICODETEXT,
                bytes: utf16le(s),
            }],
        }
    }

    #[test]
    fn empty_view_is_empty() {
        let v = Pocket::empty().view();
        assert_eq!(v.kind, PocketKind::Empty);
        assert!(v.is_empty());
    }

    // The one-shot exit barrier: a durable slot must be ON DISK when flush_durable_sync returns —
    // activate()'s worker-thread persist dies with a short-lived process (the CLI stash used to
    // report success and evaporate). Emptied durable slots must lose their file the same way.
    #[test]
    fn flush_durable_sync_mirrors_full_and_emptied_slots_before_exit() {
        let _g = crate::runroot::ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let tmp = std::env::temp_dir().join(format!("neuron-pocket-flush-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&tmp);
        let _run_pin = crate::runroot::RunDirPin::to(&tmp);

        testclip::reset_store();
        testclip::seed_slot("flushme", text_pocket("payload"), true);
        flush_durable_sync();
        assert!(
            disk_path("flushme").exists(),
            "durable slot not mirrored synchronously"
        );

        // round-trip: a fresh load (the next process) must see the payload
        testclip::reset_store();
        testclip::reload_disk();
        let found = views().iter().any(|(s, durable, v)| {
            s == "flushme" && *durable && v.text.as_deref() == Some("payload")
        });
        assert!(found, "reloaded store missing the flushed pocket");

        // emptied durable slot -> file removed by the same flush
        testclip::seed_slot("flushme", Pocket::empty(), true);
        flush_durable_sync();
        assert!(
            !disk_path("flushme").exists(),
            "emptied durable slot left a ghost file (would resurrect on next boot)"
        );

        testclip::reset_store();
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn text_view_previews_first_line() {
        let v = text_pocket("hello world\nsecond line").view();
        assert_eq!(v.kind, PocketKind::Text);
        assert_eq!(v.text.as_deref(), Some("hello world"));
        assert!(v.summary.contains("char"));
    }

    #[test]
    fn snippet_caps_and_collapses() {
        let long = "a ".repeat(100);
        let s = snippet(&long);
        assert!(s.chars().count() <= 80);
        assert!(s.ends_with('\u{2026}'));
    }

    #[test]
    fn hdrop_wide_parses_paths() {
        // DROPFILES: pFiles=20, pt(8), fNC(4), fWide=1(4) -> 20-byte header, then a wide list.
        let mut b = vec![20u8, 0, 0, 0]; // pFiles
        b.extend_from_slice(&[0; 8]); // POINT
        b.extend_from_slice(&[0; 4]); // fNC
        b.extend_from_slice(&1u32.to_le_bytes()); // fWide = true
        let mut list: Vec<u16> = "C:\\a.png".encode_utf16().collect();
        list.push(0);
        list.extend("D:\\b.txt".encode_utf16());
        list.push(0);
        list.push(0); // double-null terminator
        for u in list {
            b.extend_from_slice(&u.to_le_bytes());
        }
        let files = parse_hdrop(&b);
        assert_eq!(
            files,
            vec!["C:\\a.png".to_string(), "D:\\b.txt".to_string()]
        );
        // and the view summarizes it as files
        let v = Pocket {
            formats: vec![ClipFormat {
                id: CF_HDROP,
                bytes: b,
            }],
        }
        .view();
        assert_eq!(v.kind, PocketKind::Files);
        assert_eq!(v.files, vec!["a.png".to_string(), "b.txt".to_string()]);
    }

    #[test]
    fn dib_view_reads_dimensions() {
        // BITMAPINFOHEADER: biSize=40, biWidth=1920, biHeight=1080, ...
        let mut b = Vec::new();
        b.extend_from_slice(&40u32.to_le_bytes());
        b.extend_from_slice(&1920i32.to_le_bytes());
        b.extend_from_slice(&1080i32.to_le_bytes());
        b.extend_from_slice(&[0; 28]); // the rest of the header
        let v = Pocket {
            formats: vec![ClipFormat {
                id: CF_DIB,
                bytes: b,
            }],
        }
        .view();
        assert_eq!(v.kind, PocketKind::Image);
        assert_eq!(v.image, Some((1920, 1080)));
    }

    #[test]
    fn disk_roundtrip_preserves_all_formats() {
        let p = Pocket {
            formats: vec![
                ClipFormat {
                    id: CF_UNICODETEXT,
                    bytes: utf16le("hi"),
                },
                ClipFormat {
                    id: 0xC011,
                    bytes: vec![1, 2, 3, 4, 5],
                }, // a registered format
            ],
        };
        let mut buf = Vec::new();
        buf.extend_from_slice(MAGIC);
        buf.push(VERSION);
        put_bytes(&mut buf, b"myslot");
        buf.extend_from_slice(&(p.formats.len() as u32).to_le_bytes());
        for f in &p.formats {
            buf.extend_from_slice(&f.id.to_le_bytes());
            put_bytes(&mut buf, &f.bytes);
        }
        let (name, parsed) = parse_disk(&buf).expect("parse");
        assert_eq!(name, "myslot");
        assert_eq!(parsed, p);
    }

    #[test]
    fn sigil_is_deterministic_and_content_unique() {
        let a1 = text_pocket("hello").sigil_path(128);
        let a2 = text_pocket("hello").sigil_path(128);
        let b = text_pocket("goodbye").sigil_path(128);
        assert_eq!(a1.len(), 128);
        assert_eq!(a1, a2, "same content must draw the same sigil");
        assert_ne!(a1, b, "different content must draw a different sigil");
        assert!(Pocket::empty().sigil_path(128).is_empty());
        // stays within the normalized box
        assert!(a1
            .iter()
            .all(|&(x, y)| x.abs() <= 1.001 && y.abs() <= 1.001));
    }

    #[test]
    fn exchange_semantics_hold() {
        // The move is just (clipboard, pocket) -> (pocket, clipboard). Verify the four cases as a
        // pure exchange, independent of the OS.
        let full = text_pocket("x");
        let empty = Pocket::empty();
        // stash: live full, stored empty -> clipboard empty, pocket full
        let (to_clip, new_pocket) = (empty.clone(), full.clone());
        assert!(to_clip.is_empty() && !new_pocket.is_empty());
        // restore: live empty, stored full -> clipboard full, pocket empty
        let (to_clip, new_pocket) = (full.clone(), empty.clone());
        assert!(!to_clip.is_empty() && new_pocket.is_empty());
    }
}
