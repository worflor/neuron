//! strokelab — a developer stroke recorder, sidecarred onto Neuron for research.
//!
//! The recognizer stores only a stroke's *fingerprint* (the eigenmotion `Sig`/`Invariants`) and
//! throws the raw motion away. strokelab is the opposite end: hold **Shift** while pressing the
//! glyph **record** button and instead of teaching the vault a gesture, Neuron captures the full
//! rich weave and writes **all the data for that one stroke** to a file — nothing enters the vault.
//!
//! Per capture it emits two siblings under `./strokes/`:
//!   * `stroke_<id>.gwyph` — the canonical Whisper Glyph file (lossless points; re-encodable by the
//!     external toolchain's real codec, so eigenmotion numbers stay authoritative to *that* codec).
//!   * `stroke_<id>.json` — the bundle, in three separated sections:
//!       - `raw`         — the ground-truth signal: full-resolution points (device counts) + real
//!                         per-sample timestamps (from each motion event's `WM_INPUT` time).
//!       - `recognition` — Neuron's own engine (`glyph.rs`): the prepared/exemplar paths, the
//!                         `GestureWord` (velocity-domain, scale/speed-invariant), the per-window
//!                         eigen-fits, and — if the vault is non-empty — what this stroke matched.
//!       - `codec`       — the compression view: the raw stroke through the `engram` trajectory
//!                         codec (macro+micro cascaded oscillators, energy capture), and the gwyph.
//!
//! Nothing here touches the recognizer, the vault, or `gestures.json`. It is purely additive.

use neuron::gesture::Vault;
use neuron::glyph::{self, GlyphConfig, GlyphFit, C};
use serde::Serialize;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// Capture buffer cap for rich mode — set absurdly high so the stroke is **never** thinned
/// (`compact` would desync points from their timestamps). 2M points ≈ 33 min at 1 kHz.
pub const RICH_MAX_PTS: usize = 2_000_000;

static SEQ: AtomicU64 = AtomicU64::new(0);

// ── serializable bundle ──────────────────────────────────────────────────────────────────────

#[derive(Serialize)]
pub struct Bundle {
    schema: &'static str,
    captured_at_ms: u64,
    units: &'static str,
    raw: RawSection,
    recognition: RecognitionSection,
    codec: CodecSection,
}

#[derive(Serialize)]
struct RawSection {
    n: usize,
    /// `[x, y]` per sample, in raw device-count units (the integral of relative mouse deltas).
    points: Vec<[f64; 2]>,
    /// ms since the stroke's first sample, one per point (OS `WM_INPUT` event time). `null` if the
    /// platform/path produced none or they desynced.
    timestamps_ms: Option<Vec<u32>>,
    bbox: [f64; 4], // minx, miny, maxx, maxy
    arc_length: f64,
    duration_ms: Option<u32>,
}

#[derive(Serialize)]
struct RecognitionSection {
    config: GlyphConfig,
    /// resampled + smoothed path the signatures are computed from (`glyph::prepare`).
    prepared: Vec<[f64; 2]>,
    /// normalized unit-box exemplar polyline (`glyph::exemplar_path`).
    exemplar: Vec<[f32; 2]>,
    /// the gesture word: per-window eigen-signatures + the physical invariants.
    word: glyph::GestureWord,
    /// per-window eigen-fits (velocity domain, the recognizer's view).
    fits: Vec<FitDump>,
    /// 1-NN match against the current vault, if any glyphs are recorded.
    matched: Option<MatchDump>,
}

#[derive(Serialize)]
struct CodecSection {
    /// the raw stroke through the engram trajectory codec (macro+micro cascade).
    engram: EngramDump,
    /// sibling `.gwyph` filename + size; the canonical re-encodable stroke.
    gwyph_file: String,
    gwyph_bytes: usize,
    /// the aspect-preserving map from raw device coords to the gwyph's `[0,1]` box.
    gwyph_normalization: Normalization,
}

#[derive(Serialize)]
struct FitDump {
    k: [f64; 2],
    g: [f64; 2],
    kq: [i32; 2],
    gq: [i32; 2],
    lambda1: [f64; 2],
    lambda2: [f64; 2],
    /// dominant eigenvalue magnitude (damping) and signed angle (curvature/handedness).
    sig_mag: f64,
    sig_rot: f64,
    sig_resid_norm: f64,
    residual: f64,
    mean_step: f64,
    n: usize,
}

#[derive(Serialize)]
struct MatchDump {
    name: String,
    score: f64,
    runner_up: Option<f64>,
}

#[derive(Serialize)]
struct Normalization {
    center: [f64; 2],
    scale: f64,
    margin: f64,
    note: &'static str,
}

#[derive(Serialize)]
struct EngramDump {
    dim: usize,
    pairs: usize,
    length: usize,
    macro_block: usize,
    micro_block: usize,
    capture_pct: f64,
    total_energy: f64,
    residual_energy: f64,
    blocks: Vec<EngramBlockDump>,
}

#[derive(Serialize)]
struct EngramBlockDump {
    mode: String,
    start: usize,
    length: usize,
    macro_k: Vec<[f64; 2]>,
    macro_g: Vec<[f64; 2]>,
    micro_k: Vec<Vec<[f64; 2]>>,
    micro_g: Vec<Vec<[f64; 2]>>,
    scales: Vec<f32>,
    pair_rms: Option<Vec<f32>>,
    signal_energy: f64,
    residual_energy: f64,
    macro_capture: f64,
    micro_capture: f64,
}

// ── builders ───────────────────────────────────────────────────────────────────────────────────

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn bbox(path: &[C]) -> [f64; 4] {
    let (mut x0, mut y0, mut x1, mut y1) = (f64::MAX, f64::MAX, f64::MIN, f64::MIN);
    for c in path {
        x0 = x0.min(c.re);
        y0 = y0.min(c.im);
        x1 = x1.max(c.re);
        y1 = y1.max(c.im);
    }
    [x0, y0, x1, y1]
}

fn arc_length(path: &[C]) -> f64 {
    let mut s = 0.0;
    for w in path.windows(2) {
        s += (w[1].re - w[0].re).hypot(w[1].im - w[0].im);
    }
    s
}

/// Map a raw device-count path into the gwyph `[0,1]` box, preserving aspect and centering, with a
/// small margin so the extremes don't clamp. Returns the normalized points and the (invertible)
/// transform used.
fn normalize_aspect(path: &[C], bb: [f64; 4]) -> (Vec<[f32; 2]>, Normalization) {
    let (cx, cy) = ((bb[0] + bb[2]) * 0.5, (bb[1] + bb[3]) * 0.5);
    let scale = (bb[2] - bb[0]).max(bb[3] - bb[1]).max(1e-6);
    let margin = 0.04_f64;
    let span = 1.0 - 2.0 * margin;
    let pts = path
        .iter()
        .map(|c| {
            [
                (((c.re - cx) / scale) * span + 0.5) as f32,
                (((c.im - cy) / scale) * span + 0.5) as f32,
            ]
        })
        .collect();
    (
        pts,
        Normalization {
            center: [cx, cy],
            scale,
            margin,
            note: "norm = (raw - center)/scale * (1-2*margin) + 0.5  (aspect preserved)",
        },
    )
}

fn fit_dump(f: &GlyphFit) -> FitDump {
    let sig = glyph::signature(f);
    FitDump {
        k: [f.k.re, f.k.im],
        g: [f.g.re, f.g.im],
        kq: [f.kq.0, f.kq.1],
        gq: [f.gq.0, f.gq.1],
        lambda1: [f.lambda1.re, f.lambda1.im],
        lambda2: [f.lambda2.re, f.lambda2.im],
        sig_mag: sig.mag,
        sig_rot: sig.rot,
        sig_resid_norm: sig.resid_norm,
        residual: f.residual,
        mean_step: f.mean_step,
        n: f.n,
    }
}

fn engram_dump(path: &[C]) -> EngramDump {
    // interleaved [x0,y0,x1,y1,…] in raw coords; engram fits position-domain oscillators.
    let mut traj = Vec::with_capacity(path.len() * 2);
    for c in path {
        traj.push(c.re as f32);
        traj.push(c.im as f32);
    }
    let pk = engram::encode::encode(&traj, path.len(), 2, None);
    // map complex coefficients to [re, im] pairs — the element type (num_complex::Complex64) is
    // inferred from engram's fields, so we never have to name (or depend on) num_complex here.
    let blocks = pk
        .blocks
        .iter()
        .map(|b| EngramBlockDump {
            mode: format!("{:?}", b.mode),
            start: b.start,
            length: b.length,
            macro_k: b.macro_k.iter().map(|z| [z.re, z.im]).collect(),
            macro_g: b.macro_g.iter().map(|z| [z.re, z.im]).collect(),
            micro_k: b
                .micro_ks
                .iter()
                .map(|v| v.iter().map(|z| [z.re, z.im]).collect())
                .collect(),
            micro_g: b
                .micro_gs
                .iter()
                .map(|v| v.iter().map(|z| [z.re, z.im]).collect())
                .collect(),
            scales: b.scales.clone(),
            pair_rms: b.pair_rms.clone(),
            signal_energy: b.signal_energy,
            residual_energy: b.residual_energy,
            macro_capture: b.macro_capture,
            micro_capture: b.micro_capture,
        })
        .collect();
    EngramDump {
        dim: pk.dim,
        pairs: pk.pairs,
        length: pk.length,
        macro_block: pk.macro_block,
        micro_block: pk.micro_block,
        capture_pct: pk.capture(),
        total_energy: pk.total_energy(),
        residual_energy: pk.residual_energy(),
        blocks,
    }
}

/// Build the full bundle for a captured stroke + its per-sample timestamps.
fn build_bundle(
    path: &[C],
    stamps: &[u32],
    cfg: &GlyphConfig,
    vault: &Vault,
    gwyph_file: String,
    gwyph_bytes: usize,
    normalization: Normalization,
) -> Bundle {
    let bb = bbox(path);

    // timestamps: relative to the first sample, only if they line up 1:1 with the points.
    let (timestamps_ms, duration_ms) = if stamps.len() == path.len() && !stamps.is_empty() {
        let t0 = stamps[0];
        let rel: Vec<u32> = stamps.iter().map(|t| t.wrapping_sub(t0)).collect();
        let dur = *rel.last().unwrap();
        (Some(rel), Some(dur))
    } else {
        (None, None)
    };

    let prepared: Vec<[f64; 2]> = glyph::prepare(path, cfg)
        .iter()
        .map(|c| [c.re, c.im])
        .collect();
    let exemplar = glyph::exemplar_path(path, cfg);
    let word = glyph::analyze(path, cfg);
    let fits: Vec<FitDump> = glyph::fit_sequence(path).iter().map(fit_dump).collect();
    let matched = if vault.templates.is_empty() {
        None
    } else {
        vault.predict(&word).map(|(name, score, runner_up)| MatchDump {
            name,
            score,
            runner_up,
        })
    };

    Bundle {
        schema: "neuron.stroke.v1",
        captured_at_ms: now_ms(),
        units: "device-counts (raw mouse deltas integrated); timestamps in ms",
        raw: RawSection {
            n: path.len(),
            points: path.iter().map(|c| [c.re, c.im]).collect(),
            timestamps_ms,
            bbox: bb,
            arc_length: arc_length(path),
            duration_ms,
        },
        recognition: RecognitionSection {
            config: *cfg,
            prepared,
            exemplar,
            word,
            fits,
            matched,
        },
        codec: CodecSection {
            engram: engram_dump(path),
            gwyph_file,
            gwyph_bytes,
            gwyph_normalization: normalization,
        },
    }
}

/// Where captured strokes land (`strokes/` in the run root) — shared with the SYSTEM panel's
/// "reveal strokes" affordance so the writer and the reveal can't drift apart.
pub fn strokes_dir() -> PathBuf {
    neuron::runroot::run_root().join("strokes")
}

/// Capture-result handler: write `<dir>/stroke_<id>.{gwyph,json}` for one stroke + its timestamps.
/// Returns `(json_path, gwyph_path, n_points)` on success. A stroke shorter than 3 points has no
/// geometry to study — caller should treat that as "too short", not an error.
pub fn dump_stroke(
    path: &[C],
    stamps: &[u32],
    cfg: &GlyphConfig,
    vault: &Vault,
) -> std::io::Result<(PathBuf, PathBuf, usize)> {
    let dir = strokes_dir();
    std::fs::create_dir_all(&dir)?;

    let id = format!("{}_{}", now_ms(), SEQ.fetch_add(1, Ordering::Relaxed));
    let gwyph_name = format!("stroke_{id}.gwyph");
    let json_name = format!("stroke_{id}.json");
    let gwyph_path = dir.join(&gwyph_name);
    let json_path = dir.join(&json_name);

    // the canonical .gwyph (aspect-preserving normalized points).
    let bb = bbox(path);
    let (norm_pts, normalization) = normalize_aspect(path, bb);
    let gwyph_bytes = neuron::gwyph::encode_stroke(&norm_pts, &neuron::gwyph::StrokeStyle::default());

    // build + serialize the bundle BEFORE touching disk, so a serialize failure writes nothing.
    let bundle = build_bundle(
        path,
        stamps,
        cfg,
        vault,
        gwyph_name,
        gwyph_bytes.len(),
        normalization,
    );
    let json = serde_json::to_string_pretty(&bundle)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;

    // write the pair all-or-nothing: if the JSON write fails after the gwyph, remove the orphan so
    // there's never a lone .gwyph with no bundle beside it.
    std::fs::write(&gwyph_path, &gwyph_bytes)?;
    if let Err(e) = std::fs::write(&json_path, &json) {
        let _ = std::fs::remove_file(&gwyph_path);
        return Err(e);
    }

    Ok((json_path, gwyph_path, path.len()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn synth_spiral(n: usize) -> Vec<C> {
        (0..n)
            .map(|i| {
                let t = (i as f64 / n as f64) * std::f64::consts::TAU * 1.5;
                let r = 40.0 + 260.0 * (i as f64 / n as f64);
                C::new(800.0 + r * t.cos(), 500.0 + r * t.sin())
            })
            .collect()
    }

    /// The whole pipeline on a synthetic stroke: engram + glyph + gwyph + JSON, no live capture.
    #[test]
    fn bundle_builds_and_serializes() {
        let path = synth_spiral(90);
        let stamps: Vec<u32> = (0..90u32).map(|i| i * 2).collect(); // ~2ms/sample
        let cfg = GlyphConfig::default();
        let vault = Vault::default();

        let bb = bbox(&path);
        let (norm_pts, normalization) = normalize_aspect(&path, bb);
        let gwyph = neuron::gwyph::encode_stroke(&norm_pts, &neuron::gwyph::StrokeStyle::default());
        assert!(gwyph.len() > 16, "gwyph bytes produced");

        let bundle = build_bundle(
            &path,
            &stamps,
            &cfg,
            &vault,
            "stroke_test.gwyph".into(),
            gwyph.len(),
            normalization,
        );
        let json = serde_json::to_string_pretty(&bundle).expect("serializes");

        // the three sections + their signature fields are all present.
        for key in [
            "\"raw\"",
            "\"recognition\"",
            "\"codec\"",
            "timestamps_ms",
            "winding",
            "engram",
            "macro_capture",
            "sig_rot",
        ] {
            assert!(json.contains(key), "bundle JSON missing {key}");
        }
        // timestamps lined up 1:1 with points → present, not null.
        assert!(
            json.contains("\"duration_ms\": 178"),
            "expected duration 178ms from the synthetic stamps"
        );
        // engram produced a real decomposition (at least one block, finite capture).
        assert!(bundle.codec.engram.length == 90);
        assert!(bundle.codec.engram.capture_pct.is_finite());
        assert!(!bundle.recognition.fits.is_empty(), "per-window eigen-fits");
    }
}
