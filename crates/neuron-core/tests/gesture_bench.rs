// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

//! Deterministic ignored microbench for the public gesture recognition path.
//! Run with: cargo test -p neuron --test gesture_bench -- --ignored --nocapture

use std::alloc::{GlobalAlloc, Layout, System};
use std::hint::black_box;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Instant;

use neuron::gesture::{Template, Vault};
use neuron::glyph::{GestureWord, GlyphConfig, Invariants, Sig};

struct CountingAllocator;

static COUNTING: AtomicBool = AtomicBool::new(false);
static ALLOCS: AtomicUsize = AtomicUsize::new(0);
static BYTES: AtomicUsize = AtomicUsize::new(0);

// SAFETY: delegates every allocation operation to the system allocator unchanged.
unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let ptr = System.alloc(layout);
        if !ptr.is_null() && COUNTING.load(Ordering::Relaxed) {
            ALLOCS.fetch_add(1, Ordering::Relaxed);
            BYTES.fetch_add(layout.size(), Ordering::Relaxed);
        }
        ptr
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        System.dealloc(ptr, layout);
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let new_ptr = System.realloc(ptr, layout, new_size);
        if !new_ptr.is_null() && COUNTING.load(Ordering::Relaxed) {
            ALLOCS.fetch_add(1, Ordering::Relaxed);
            BYTES.fetch_add(new_size, Ordering::Relaxed);
        }
        new_ptr
    }
}

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

const LENGTHS: [usize; 3] = [16, 32, 64];
const VAULT_SIZES: [usize; 4] = [1, 8, 32, 128];
const SAMPLES: usize = 100;

fn word(len: usize, seed: usize) -> GestureWord {
    let sigs = (0..len)
        .map(|i| {
            let x = (i * 17 + seed * 13) as f64;
            Sig {
                mag: 0.2 + (x.sin() + 1.0) * 0.3,
                rot: (x * 0.071).sin() * std::f64::consts::PI,
                resid_norm: (x * 0.13).cos().abs(),
            }
        })
        .collect();
    GestureWord {
        sigs,
        inv: Invariants {
            winding: (seed % 5) as f64 * 0.25,
            bending: 1.0 + seed as f64 * 0.01,
            closure: (seed % 9) as f64 * 0.1,
        },
    }
}

#[test]
#[ignore = "performance harness; run explicitly with --ignored --nocapture"]
fn gesture_recognition_latency_and_allocations() {
    let cfg = GlyphConfig::default();
    let mut rows = Vec::new();

    for len in LENGTHS {
        let query = word(len, 999);
        for vault_size in VAULT_SIZES {
            let templates = (0..vault_size)
                .map(|i| Template {
                    name: format!("template-{i}"),
                    word: word(len, i),
                    exemplar: Vec::new(),
                })
                .collect();
            let vault = Vault { config: cfg, templates };

            for _ in 0..20 {
                black_box(vault.recognize(black_box(&query)));
            }
            let mut nanos = Vec::with_capacity(SAMPLES);
            ALLOCS.store(0, Ordering::Relaxed);
            BYTES.store(0, Ordering::Relaxed);
            COUNTING.store(true, Ordering::SeqCst);
            for _ in 0..SAMPLES {
                let start = Instant::now();
                black_box(vault.recognize(black_box(&query)));
                nanos.push(start.elapsed().as_nanos() as f64);
            }
            COUNTING.store(false, Ordering::SeqCst);
            nanos.sort_by(f64::total_cmp);
            rows.push((
                len,
                vault_size,
                nanos.iter().sum::<f64>() / SAMPLES as f64,
                nanos[SAMPLES / 2],
                nanos[SAMPLES * 95 / 100],
                nanos[SAMPLES - 1],
                ALLOCS.load(Ordering::Relaxed) as f64 / SAMPLES as f64,
                BYTES.load(Ordering::Relaxed) as f64 / SAMPLES as f64,
            ));
        }
    }

    println!("\nGesture Vault::recognize — {SAMPLES} calls/case; synthetic fixed signatures");
    println!("{:<8} {:<8} {:>10} {:>10} {:>10} {:>10} {:>12} {:>12}", "sigs", "vault", "avg_ns", "p50_ns", "p95_ns", "max_ns", "allocs/call", "bytes/call");
    for (len, size, avg, p50, p95, max, allocs, bytes) in rows {
        println!("{len:<8} {size:<8} {avg:>10.0} {p50:>10.0} {p95:>10.0} {max:>10.0} {allocs:>12.2} {bytes:>12.0}");
    }
}
