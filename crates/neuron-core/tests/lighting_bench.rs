//! Part A — deterministic micro-bench of the lighting render path (NO GUI).
//!
//! Times, in MICROSECONDS, with a warmup then N≈2000 iterations (avg · p50 · max):
//!   * each of the 15 tile effects' `FrameGen::frame(6, 22, t, base)` individually, and
//!   * each live provider's steady-state getter (`audio_level::level`, `sys_stats::cpu/ram`,
//!     `screen_ambient::grid`) — `ensure()`d and warmed first, since the getter is what the
//!     render path calls every tile.
//!
//! Ignored by default (it sleeps to warm the providers + prints a table). Run it with:
//!   cargo test -p neuron --test lighting_bench -- --ignored --nocapture
//!
//! 6×22 is the real BlackWidow Chroma V2 matrix (132 cells), so these numbers are the per-call
//! cost the live page pays.

use std::hint::black_box;
use std::time::{Duration, Instant};

use neuron::effects::{self, EffectParams};
use neuron::lighting::Rgb;

const ROWS: u8 = 6;
const COLS: u8 = 22;
const N: usize = 2000;
const WARMUP: usize = 200;

/// The 15 effect tiles the lighting grid renders every tick (the catalog minus the data tile).
const EFFECTS: &[&str] = &[
    "static",
    "breathing",
    "spectrum",
    "wave",
    "aurora",
    "fire",
    "cascade",
    "comet",
    "starlight",
    "reactive",
    "ripple",
    "colorwheel",
    "audiometer",
    "pulse",
    "ambient",
];

struct Stat {
    name: String,
    avg_us: f64,
    p50_us: f64,
    max_us: f64,
}

/// Time `f` N times (after WARMUP), returning avg/p50/max in microseconds. `t` is advanced each
/// iteration so time-driven effects don't see a frozen clock.
fn bench<F: FnMut(f32)>(name: &str, mut f: F) -> Stat {
    for i in 0..WARMUP {
        f(i as f32 * 0.016);
    }
    let mut samples: Vec<f64> = Vec::with_capacity(N);
    for i in 0..N {
        let t = (WARMUP + i) as f32 * 0.016; // ~60fps-spaced time so the sim advances realistically
        let start = Instant::now();
        f(t);
        samples.push(start.elapsed().as_nanos() as f64 / 1000.0);
    }
    let avg = samples.iter().sum::<f64>() / samples.len() as f64;
    samples.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let p50 = samples[samples.len() / 2];
    let max = *samples.last().unwrap();
    Stat { name: name.to_string(), avg_us: avg, p50_us: p50, max_us: max }
}

#[test]
#[ignore = "perf micro-bench; run explicitly with --ignored --nocapture"]
fn part_a_lighting_microbench() {
    let base = Rgb::new(74, 242, 176); // a representative weave accent
    let mut stats: Vec<Stat> = Vec::new();

    // ── 1) the 15 effects' frame() ──
    for &slug in EFFECTS {
        let mut gen = effects::make_with(slug, EffectParams::default())
            .unwrap_or_else(|| panic!("effect '{slug}' should resolve"));
        let s = bench(&format!("effect:{slug}"), |t| {
            black_box(gen.frame(ROWS, COLS, t, base));
        });
        stats.push(s);
    }

    // ── 2) the live providers — ensure + warm, then time the GETTER (what the render path calls) ──
    neuron::audio_level::ensure("speakers");
    neuron::sys_stats::ensure();
    neuron::screen_ambient::ensure();
    // give each sampler thread time to publish at least one (sys_stats needs two 1Hz ticks).
    std::thread::sleep(Duration::from_millis(2300));

    stats.push(bench("provider:audio_level::level", |_| {
        black_box(neuron::audio_level::level());
    }));
    stats.push(bench("provider:sys_stats::cpu", |_| {
        black_box(neuron::sys_stats::cpu());
    }));
    stats.push(bench("provider:sys_stats::ram", |_| {
        black_box(neuron::sys_stats::ram());
    }));
    stats.push(bench("provider:screen_ambient::grid (lock+clone)", |_| {
        black_box(neuron::screen_ambient::grid());
    }));

    // ── ranked table (slowest first by avg) ──
    stats.sort_by(|a, b| b.avg_us.partial_cmp(&a.avg_us).unwrap());
    println!("\n=== PART A — per-call cost @ 6x22 (132 cells), N={N} ===");
    println!("{:<44} {:>10} {:>10} {:>10}", "name", "avg_us", "p50_us", "max_us");
    println!("{}", "-".repeat(78));
    for s in &stats {
        println!("{:<44} {:>10.3} {:>10.3} {:>10.3}", s.name, s.avg_us, s.p50_us, s.max_us);
    }
    println!("{}", "-".repeat(78));
    let sum_effects: f64 = stats
        .iter()
        .filter(|s| s.name.starts_with("effect:"))
        .map(|s| s.avg_us)
        .sum();
    println!("sum of all 15 effects' avg frame() = {sum_effects:.3} us\n");
}
