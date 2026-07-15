//! TASK 2 (first half): real concurrent claim fights through the actor.
//!
//! Every arbiter/shell test elsewhere in this crate is single-threaded —
//! concurrent arrival has zero coverage before this file. This drives genuine
//! multi-thread arrival at one surface through cloned `HostHandle`s, racing
//! the shell's own periodic sweeper (`shell::SWEEP_PERIOD`, every 250ms).
//!
//! The kernel-poisoning half of TASK 2 ("claim fight while the kernel is
//! poisoned mid-fight") lives in `neuron-host/src/shell.rs`'s own test module
//! instead of here: `Cmd::Poison`/`HostHandle::poison` are `#[cfg(test)]`-gated
//! INSIDE the crate, and this file is an external integration-test binary
//! that links the lib WITHOUT `--cfg test` — those items are simply not
//! visible from here. See `shell::tests::claim_fight_survives_poisoning_mid_fight`.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use neuron_host::api::{HostApi, LeaseSpec, SurfaceInfo, SurfaceKind};
use neuron_host::arbiter::{band, Content, Rgb, SourceId};
use neuron_host::shell::{Host, SWEEP_PERIOD};

const THREADS: usize = 8;
const ITERS: usize = 50;
const BANDS: [i32; 4] = [band::BASE, band::AMBIENT, band::SESSION, band::OVERRIDE];

#[test]
fn concurrent_claim_fight_resolves_consistently_with_no_panic_and_unique_ids() {
    let host = Host::spawn();
    let mut setup = host.handle();
    setup.declare(SurfaceInfo::grid("kbd", "Board", SurfaceKind::Keyboard, 1, 1));

    let seen_ids: Arc<Mutex<HashSet<SourceId>>> = Arc::new(Mutex::new(HashSet::new()));
    let color_of: Arc<Mutex<HashMap<SourceId, Rgb>>> = Arc::new(Mutex::new(HashMap::new()));

    let workers: Vec<_> = (0..THREADS)
        .map(|t| {
            let mut h = host.handle();
            let seen_ids = seen_ids.clone();
            let color_of = color_of.clone();
            thread::Builder::new()
                .name(format!("claim-fight-{t}"))
                .spawn(move || {
                    // Fixed band per thread, distinct owners every iteration —
                    // "distinct owners and bands" racing one surface.
                    let band = BANDS[t % BANDS.len()];
                    for i in 0..ITERS {
                        let owner = h.next_source();
                        assert!(
                            seen_ids.lock().unwrap().insert(owner),
                            "source id {owner:?} reused under concurrency"
                        );
                        let color =
                            Rgb((t as u8).wrapping_mul(31).wrapping_add(i as u8), t as u8, i as u8);
                        color_of.lock().unwrap().insert(owner, color);
                        let lease = if i % 3 == 0 {
                            LeaseSpec::Ttl(Duration::from_millis(15))
                        } else {
                            LeaseSpec::Pinned
                        };
                        let id =
                            h.claim("kbd", owner, band, lease, Content::Fill(color), Instant::now());
                        if let Some(id) = id {
                            if i % 2 == 0 {
                                h.refresh(id, Instant::now());
                            }
                            if i % 5 == 0 {
                                h.release(id);
                            }
                        }
                    }
                })
                .expect("spawn fighting thread")
        })
        .collect();

    for w in workers {
        assert!(w.join().is_ok(), "a fighting thread must never panic");
    }

    // Let the shell's own periodic sweeper (SWEEP_PERIOD) catch every lapsed
    // heartbeat claim before reading the quiesced state — the sweeper races
    // the fight the whole time it runs (it never stops), this just waits for
    // it to have had a couple of passes since the last claim landed.
    thread::sleep(SWEEP_PERIOD * 2);

    let mut h = host.handle();
    let now = Instant::now();
    let claims = h.claims("kbd", now);
    let frame = h.resolve("kbd", now).expect("surface exists");
    let colors = color_of.lock().unwrap();
    match claims.first() {
        Some(top) => {
            let expected = colors.get(&top.owner).copied();
            assert_eq!(
                frame[0], expected,
                "resolve()'s winner must be the SAME claim claims() reports as topmost \
                 (both independently derived from the same internal layer stack)"
            );
        }
        None => {
            assert_eq!(frame[0], None, "no alive claims ⇒ resolve must show nothing");
        }
    }

    assert_eq!(
        seen_ids.lock().unwrap().len(),
        THREADS * ITERS,
        "every issued source id must be unique, even issued concurrently across threads"
    );
}
