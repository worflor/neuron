// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

//! The host shell — the kernel as an immortal actor.
//!
//! One thread owns the [`Kernel`]. Everyone else holds a [`HostHandle`]
//! (cloneable) and speaks [`HostApi`] over a channel — there is no shared
//! mutex on kernel state, so there is nothing to poison; a panicking caller
//! can never corrupt the kernel, and a kernel fault can never deadlock a
//! caller (their reply sender drops, they get a default, they carry on).
//!
//! Process recovery is composed from the pieces already proven in
//! isolation:
//!
//! - the actor loop runs inside `catch_unwind`; the command RECEIVER lives
//!   outside it, so commands queued during a fault survive and are served by
//!   the reborn kernel (the command that *caused* the fault is consumed — its
//!   caller sees a dropped reply, which is the honest answer);
//! - rebirth replays the SEED (declared surfaces only). Leased claims are
//!   deliberately NOT reborn: sessions must re-claim, exactly the lease
//!   contract, so a fault can never resurrect a dead session's paint. There is
//!   no data journal behind this: every owner (app base, Chroma REST, Chroma
//!   SHM, `OpenRGB`) already carries its own tested reassert/recovery path —
//!   a REST heartbeat, an `OpenRGB` idle-tick reassert, an SHM refresh, an
//!   app-base heartbeat — and all of it is `Content::Live` (closures), which
//!   a data log cannot replay by construction. Recovery is owner-driven by
//!   design, not seed-driven;
//! - the [`governor`](crate::governor) paces rebirths: isolated faults restart
//!   in under a second, a fault storm escalates and the shell gives up loudly
//!   rather than spinning — spectral radius < 1, enforced at construction.
//!
//! The shell also owns the sweep cadence: expired leases are pruned every
//! [`SWEEP_PERIOD`] and each release is published on the bus as
//! `host.layer.released` — teardown stays observable even when nobody is
//! resolving frames.

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError, Sender};
use std::thread;
use std::time::{Duration, Instant};

use crate::api::{HostApi, LeaseSpec, SurfaceInfo};
use crate::arbiter::{Content, LayerId, Rgb, SourceId};
use crate::bus::{Signal, Value};
use crate::governor::{Config, Governor, Verdict};
use crate::Kernel;

pub const SWEEP_PERIOD: Duration = Duration::from_millis(250);

pub enum Cmd {
    Declare { info: SurfaceInfo },
    Surfaces { reply: Sender<Vec<SurfaceInfo>> },
    NextSource { reply: Sender<SourceId> },
    Claim {
        surface: String,
        owner: SourceId,
        priority: i32,
        lease: LeaseSpec,
        content: Content,
        now: Instant,
        reply: Sender<Option<LayerId>>,
    },
    SetContent { id: LayerId, content: Content, now: Instant, reply: Sender<bool> },
    Refresh { id: LayerId, now: Instant, reply: Sender<bool> },
    Release { id: LayerId },
    ReleaseOwner { owner: SourceId },
    Resolve { surface: String, now: Instant, reply: Sender<Option<Vec<Option<Rgb>>>> },
    Claims { surface: String, now: Instant, reply: Sender<Vec<Claim>> },
    Label { owner: SourceId, name: String },
    Publish { path: String, value: Value },
    Subscribe { prefix: String, reply: Sender<Receiver<Signal>> },
    /// Test-only: makes the kernel thread panic, to prove rebirth works.
    #[cfg(test)]
    Poison,
    Shutdown,
}

/// One alive claim on a surface, as the GUI sees it: who, how strongly, and —
/// when the adapter told us — by NAME. The ownership truth no last-writer-wins
/// tool can even ask for.
#[derive(Clone, Debug, PartialEq)]
pub struct Claim {
    pub owner: SourceId,
    pub priority: i32,
    pub label: Option<String>,
}

/// The running host: owns the kernel thread. Dropping it shuts the actor down
/// cleanly (join), so a scoped Host in a test can't leak a thread.
pub struct Host {
    tx: Sender<Cmd>,
    thread: Option<thread::JoinHandle<()>>,
}

impl Host {
    #[must_use]
    pub fn spawn() -> Host {
        let (tx, rx) = channel();
        let thread = crate::worker::spawn_named("neuron-host-kernel", move || run(rx))
            .expect("spawn kernel actor");
        Host { tx, thread: Some(thread) }
    }

    #[must_use]
    pub fn handle(&self) -> HostHandle {
        HostHandle { tx: self.tx.clone() }
    }
}

/// Bound on [`Host`]'s teardown. The kernel actor normally consumes `Cmd::Shutdown` within one
/// command-loop turn — but a turn can include `Arbiter::resolve`, which invokes arbitrary
/// [`LiveContent::render`] implementations SYNCHRONOUSLY on the actor thread. A wedged renderer
/// (an adapter's live layer stuck on a lock or a blocking call) would leave `Cmd::Shutdown`
/// forever unread, and an unbounded join here would hang process teardown at the CENTRAL kernel
/// owner — the one place the bounded-teardown sweep must not have a hole. 2s is many orders of
/// magnitude above a real resolve (microseconds over a few hundred cells); hitting this deadline
/// means a renderer is genuinely wedged, and leaking the kernel thread at exit beats hanging.
const HOST_DROP_DEADLINE: Duration = Duration::from_secs(2);

/// Bound on one [`HostHandle`] request's reply wait — the caller-side twin of
/// [`HOST_DROP_DEADLINE`]: a leaked-but-wedged kernel still owns the command Receiver, so sends
/// succeed and only this deadline stands between a retained handle and an infinite wait (see
/// `request`). Same wedge-not-slowness sizing rationale.
const HOST_REQUEST_DEADLINE: Duration = Duration::from_secs(2);

impl Drop for Host {
    fn drop(&mut self) {
        let _ = self.tx.send(Cmd::Shutdown);
        if let Some(t) = self.thread.take() {
            crate::worker::join_bounded(t, HOST_DROP_DEADLINE, "neuron-host-kernel");
        }
    }
}

/// Cloneable channel-backed [`HostApi`]. Every method degrades honestly if the
/// kernel is gone (escalated or shutting down): reads return empty/None/false,
/// writes are dropped — callers observe absence, never hang forever.
#[derive(Clone)]
pub struct HostHandle {
    tx: Sender<Cmd>,
}

impl HostHandle {
    fn request<T>(&self, build: impl FnOnce(Sender<T>) -> Cmd) -> Option<T> {
        let (rtx, rrx) = channel();
        self.tx.send(build(rtx)).ok()?;
        // Bounded, not bare `recv()`: the disconnected-channel case (kernel escalated or shut
        // down cleanly) errors immediately, but a kernel thread that is ALIVE-BUT-WEDGED — stuck
        // inside an adapter's `LiveContent::render`, or leaked by `Host::drop`'s bounded join —
        // still owns the command Receiver, so the send above SUCCEEDS and an unbounded recv would
        // wait forever for a reply the actor can never produce. That would quietly move the hang
        // this type's contract forbids ("callers observe absence, never hang forever") from Drop
        // into every later API call on a retained handle. A real kernel turn is microseconds;
        // this deadline is the same wedge-not-slowness class as `HOST_DROP_DEADLINE`, and timing
        // out degrades to the documented honest absence (`None`).
        rrx.recv_timeout(HOST_REQUEST_DEADLINE).ok()
    }

    /// Subscribe to bus signals by prefix (see [`crate::bus::Bus::subscribe`]).
    /// The receiver closes on kernel rebirth — resubscribe on disconnect.
    #[must_use]
    pub fn subscribe(&self, prefix: &str) -> Option<Receiver<Signal>> {
        self.request(|reply| Cmd::Subscribe { prefix: prefix.into(), reply })
    }

    /// The alive claims on a surface, topmost first, with adapter-provided
    /// names attached — the "who is controlling this board" readout. Empty if
    /// the kernel is gone or the surface unknown.
    #[must_use]
    pub fn claims(&self, surface: &str, now: Instant) -> Vec<Claim> {
        self.request(|reply| Cmd::Claims { surface: surface.into(), now, reply })
            .unwrap_or_default()
    }

    #[cfg(test)]
    fn poison(&self) {
        let _ = self.tx.send(Cmd::Poison);
    }
}

impl HostApi for HostHandle {
    fn declare(&mut self, info: SurfaceInfo) {
        let _ = self.tx.send(Cmd::Declare { info });
    }

    fn surfaces(&mut self) -> Vec<SurfaceInfo> {
        self.request(|reply| Cmd::Surfaces { reply }).unwrap_or_default()
    }

    fn next_source(&mut self) -> SourceId {
        // Kernel gone → a source id that owns nothing; subsequent claims fail
        // honestly through the same dead channel.
        self.request(|reply| Cmd::NextSource { reply }).unwrap_or(SourceId(u64::MAX))
    }

    fn claim(
        &mut self,
        surface: &str,
        owner: SourceId,
        priority: i32,
        lease: LeaseSpec,
        content: Content,
        now: Instant,
    ) -> Option<LayerId> {
        self.request(|reply| Cmd::Claim {
            surface: surface.into(),
            owner,
            priority,
            lease,
            content,
            now,
            reply,
        })
        .flatten()
    }

    fn set_content(&mut self, id: LayerId, content: Content, now: Instant) -> bool {
        self.request(|reply| Cmd::SetContent { id, content, now, reply }).unwrap_or(false)
    }

    fn refresh(&mut self, id: LayerId, now: Instant) -> bool {
        self.request(|reply| Cmd::Refresh { id, now, reply }).unwrap_or(false)
    }

    fn release(&mut self, id: LayerId) {
        let _ = self.tx.send(Cmd::Release { id });
    }

    fn release_owner(&mut self, owner: SourceId) {
        let _ = self.tx.send(Cmd::ReleaseOwner { owner });
    }

    fn resolve(&mut self, surface: &str, now: Instant) -> Option<Vec<Option<Rgb>>> {
        self.request(|reply| Cmd::Resolve { surface: surface.into(), now, reply }).flatten()
    }

    fn publish(&mut self, path: &str, value: Value) {
        let _ = self.tx.send(Cmd::Publish { path: path.into(), value });
    }

    fn label_source(&mut self, owner: SourceId, name: &str) {
        let _ = self.tx.send(Cmd::Label { owner, name: name.into() });
    }
}

enum Flow {
    Stop,
}

fn run(rx: Receiver<Cmd>) {
    // The rebirth seed: declared surfaces, deduped by key. Deliberately NOT
    // claims — see module docs.
    let mut seed: Vec<SurfaceInfo> = Vec::new();
    // Source IDs must be unique for the WHOLE life of the host, not per kernel
    // incarnation. OpenRGB/Chroma sessions live OUTSIDE the actor and keep using
    // their old owner id across a rebirth (the handle contract: queued commands
    // are served by the reborn kernel, connections survive). A reborn kernel that
    // reset its counter to 1 would hand a live session's id to a brand-new
    // connection — and `release_owner` is owner-wide, so either side's disconnect
    // or timeout would then drop BOTH sessions' claims. Carry the high-water mark
    // across rebirths so an id is never reused while its original owner may live.
    let mut next_source: u64 = 1;
    let mut governor =
        Governor::new(Config::critically_damped(0.8)).expect("default governor config is stable");
    loop {
        let mut kernel = Kernel::new();
        kernel.next_source = next_source;
        for info in seed.clone() {
            kernel.declare(info);
        }
        let outcome = catch_unwind(AssertUnwindSafe(|| serve(&mut kernel, &rx, &mut seed)));
        // Preserve the counter across rebirth. Readable even after a fault: the
        // borrow ends when `catch_unwind` returns, and a panic can't corrupt a
        // plain `u64` — worst case a fault mid-`next_source()` leaves it one short
        // of incremented, still >= every id ever issued, so never reused.
        next_source = kernel.next_source;
        match outcome {
            Ok(Flow::Stop) => break,
            Err(_) => match governor.on_crash(Instant::now()) {
                Verdict::RestartAfter(delay) => {
                    eprintln!(
                        "neuron-host: kernel fault contained; rebirth in {delay:?} \
                         (crashes: {}, absorbed: {})",
                        governor.ledger.crashes, governor.ledger.absorbed
                    );
                    thread::sleep(delay);
                }
                Verdict::Escalate => {
                    eprintln!(
                        "neuron-host: kernel fault storm ({} crashes) — escalating: actor exiting",
                        governor.ledger.crashes
                    );
                    break;
                }
            },
        }
    }
}

fn serve(kernel: &mut Kernel, rx: &Receiver<Cmd>, seed: &mut Vec<SurfaceInfo>) -> Flow {
    let mut last_sweep = Instant::now();
    loop {
        match rx.recv_timeout(SWEEP_PERIOD) {
            Ok(Cmd::Shutdown) => return Flow::Stop,
            Ok(cmd) => apply(kernel, seed, cmd),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return Flow::Stop,
        }
        let now = Instant::now();
        if now.duration_since(last_sweep) >= SWEEP_PERIOD {
            last_sweep = now;
            for r in kernel.arbiter.sweep(now) {
                kernel.bus.publish(
                    "host.layer.released",
                    Value::Text(format!("{}/{}", r.surface, r.owner.0)),
                );
            }
        }
    }
}

fn apply(kernel: &mut Kernel, seed: &mut Vec<SurfaceInfo>, cmd: Cmd) {
    match cmd {
        Cmd::Declare { info } => {
            // Seed first, kernel second: if the declare itself faults, rebirth
            // retries it (and a poisoned declaration escalates via the
            // governor rather than being silently forgotten).
            if let Some(existing) = seed.iter_mut().find(|i| i.key == info.key) {
                *existing = info.clone();
            } else {
                seed.push(info.clone());
            }
            kernel.declare(info);
        }
        Cmd::Surfaces { reply } => {
            let _ = reply.send(kernel.surfaces());
        }
        Cmd::NextSource { reply } => {
            let _ = reply.send(kernel.next_source());
        }
        Cmd::Claim { surface, owner, priority, lease, content, now, reply } => {
            let _ = reply.send(kernel.claim(&surface, owner, priority, lease, content, now));
        }
        Cmd::SetContent { id, content, now, reply } => {
            let _ = reply.send(kernel.set_content(id, content, now));
        }
        Cmd::Refresh { id, now, reply } => {
            let _ = reply.send(kernel.refresh(id, now));
        }
        Cmd::Release { id } => kernel.release(id),
        Cmd::ReleaseOwner { owner } => kernel.release_owner(owner),
        Cmd::Resolve { surface, now, reply } => {
            let _ = reply.send(kernel.resolve(&surface, now));
        }
        Cmd::Claims { surface, now, reply } => {
            let claims = kernel
                .arbiter
                .claims(&surface, now)
                .into_iter()
                .map(|(owner, priority)| Claim {
                    owner,
                    priority,
                    label: kernel.label_of(owner).map(str::to_string),
                })
                .collect();
            let _ = reply.send(claims);
        }
        Cmd::Label { owner, name } => kernel.label_source(owner, &name),
        Cmd::Publish { path, value } => kernel.publish(&path, value),
        Cmd::Subscribe { prefix, reply } => {
            let _ = reply.send(kernel.bus.subscribe(&prefix));
        }
        #[cfg(test)]
        Cmd::Poison => panic!("test poison"),
        Cmd::Shutdown => unreachable!("handled in serve"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::SurfaceKind;
    use crate::arbiter::band;

    #[test]
    fn host_drop_is_bounded_when_a_live_renderer_wedges() {
        // `Arbiter::resolve` runs arbitrary `LiveContent::render` implementations SYNCHRONOUSLY on
        // the kernel actor thread — so a wedged renderer (stuck lock, blocking call) leaves
        // `Cmd::Shutdown` forever unread. This pins the `HOST_DROP_DEADLINE` backstop: dropping
        // `Host` must return within the bound even with the kernel mid-wedge, leaking the actor
        // thread (loudly) instead of hanging process teardown at the central kernel owner.
        use crate::arbiter::{Content, LiveContent};
        struct Wedged;
        impl LiveContent for Wedged {
            fn render(&mut self, _now: Instant) -> Vec<Option<Rgb>> {
                // Park forever: an unpaired Condvar wait, the same shape as the writer lane's
                // BlockingSink. The kernel thread is deliberately leaked by the bounded drop.
                let pair = std::sync::Mutex::new(());
                let cv = std::sync::Condvar::new();
                let g = pair.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                drop(cv.wait(g));
                unreachable!("nobody notifies");
            }
            fn boxed_clone(&self) -> Box<dyn LiveContent> {
                Box::new(Wedged)
            }
        }

        let host = Host::spawn();
        let mut h = host.handle();
        h.declare(SurfaceInfo::grid("kbd", "Board", SurfaceKind::Keyboard, 1, 1));
        let owner = h.next_source();
        h.claim(
            "kbd",
            owner,
            band::SESSION,
            LeaseSpec::Pinned,
            Content::Live(Box::new(Wedged)),
            Instant::now(),
        )
        .expect("claim the wedged live layer");
        // Fire a resolve the kernel will wedge inside; never read the reply.
        let (tx, _rx) = std::sync::mpsc::channel();
        let _ = host.tx.send(Cmd::Resolve { surface: "kbd".into(), now: Instant::now(), reply: tx });
        std::thread::sleep(Duration::from_millis(150)); // let the actor actually enter render()

        let started = Instant::now();
        drop(host);
        assert!(
            started.elapsed() < HOST_DROP_DEADLINE * 3,
            "Host::drop must be bounded with a wedged renderer, took {:?}",
            started.elapsed()
        );

        // The sharper half of the contract: the wedged kernel thread was LEAKED, not killed — it
        // still owns the command Receiver, so sends from this retained handle SUCCEED and only
        // `request`'s reply deadline stands between the caller and an infinite wait. A retained
        // handle after teardown must observe honest absence within the bound, never hang.
        let post = Instant::now();
        let surfaces = h.surfaces();
        assert!(
            post.elapsed() < HOST_REQUEST_DEADLINE * 3,
            "a retained HostHandle must not hang against a leaked wedged kernel, took {:?}",
            post.elapsed()
        );
        assert!(
            surfaces.is_empty(),
            "a wedged kernel can produce no reply — the handle must read honest absence"
        );
    }

    #[test]
    fn claims_carry_names_and_bands_the_who_wins_readout() {
        // The emergent config no last-writer-wins tool can represent: name a
        // session, and read whether it WINS or is SUPPRESSED purely from band
        // ordering — the exact query the LIGHTING truth strip runs.
        let host = Host::spawn();
        let mut h = host.handle();
        h.declare(SurfaceInfo::grid("kbd", "Board", SurfaceKind::Keyboard, 1, 1));
        let now = Instant::now();

        let base = h.next_source();
        let game = h.next_source();
        h.label_source(game, "Overwatch");

        // Policy = games take over: base at BASE, game session above it.
        let base_lo =
            h.claim("kbd", base, band::BASE, LeaseSpec::Pinned, Content::Fill(Rgb(0, 40, 30)), now)
                .unwrap();
        h.claim("kbd", game, band::SESSION, LeaseSpec::Pinned, Content::Fill(Rgb(200, 0, 0)), now)
            .unwrap();
        let claims = h.claims("kbd", now);
        assert_eq!(claims[0].owner, game, "the named game is topmost");
        assert_eq!(claims[0].label.as_deref(), Some("Overwatch"));
        assert!(claims[0].priority > band::BASE, "game paints above base ⇒ PAINTING");

        // Flip to "my lighting wins": re-pin the base ABOVE sessions. The SAME
        // named game is still present but now LOSES — the suppressed state,
        // which no arbiter-less tool can even surface.
        h.release(base_lo);
        h.claim("kbd", base, band::OVERRIDE, LeaseSpec::Pinned, Content::Fill(Rgb(0, 40, 30)), now)
            .unwrap();
        let claims = h.claims("kbd", now);
        assert_eq!(claims[0].owner, base, "base at OVERRIDE wins");
        let game_claim = claims.iter().find(|c| c.owner == game).expect("game still present");
        assert_eq!(game_claim.label.as_deref(), Some("Overwatch"));
        assert!(game_claim.priority < band::OVERRIDE, "game below base ⇒ SUPPRESSED, not gone");
    }

    #[test]
    fn round_trip_through_the_actor() {
        let host = Host::spawn();
        let mut h = host.handle();
        h.declare(SurfaceInfo::grid("kbd", "Board", SurfaceKind::Keyboard, 2, 3));
        let owner = h.next_source();
        let now = Instant::now();
        let id = h
            .claim("kbd", owner, band::SESSION, LeaseSpec::Pinned, Content::Fill(Rgb(9, 8, 7)), now)
            .expect("claim through channel");
        let frame = h.resolve("kbd", now).expect("resolve through channel");
        assert_eq!(frame, vec![Some(Rgb(9, 8, 7)); 6]);
        assert!(h.set_content(id, Content::Fill(Rgb(1, 1, 1)), now));
        assert_eq!(h.resolve("kbd", now).unwrap()[0], Some(Rgb(1, 1, 1)));
    }

    #[test]
    fn sweep_publishes_observable_teardown() {
        let host = Host::spawn();
        let mut h = host.handle();
        h.declare(SurfaceInfo::grid("kbd", "Board", SurfaceKind::Keyboard, 1, 1));
        let events = host.handle().subscribe("host.layer").expect("subscribe");
        let owner = h.next_source();
        h.claim(
            "kbd",
            owner,
            band::SESSION,
            LeaseSpec::Ttl(Duration::from_millis(50)),
            Content::Fill(Rgb(1, 2, 3)),
            Instant::now(),
        )
        .unwrap();
        // Within a sweep period + ttl, the lapse must be announced on the bus.
        let sig = events.recv_timeout(Duration::from_secs(2)).expect("released event");
        assert_eq!(sig.path, "host.layer.released");
        match sig.value {
            Value::Text(t) => assert!(t.starts_with("kbd/"), "unexpected payload {t}"),
            v => panic!("unexpected value {v:?}"),
        }
    }

    #[test]
    fn kernel_fault_rebirths_with_surfaces_but_without_claims() {
        let host = Host::spawn();
        let mut h = host.handle();
        h.declare(SurfaceInfo::grid("kbd", "Board", SurfaceKind::Keyboard, 1, 1));
        let owner = h.next_source();
        h.claim(
            "kbd",
            owner,
            band::SESSION,
            LeaseSpec::Pinned,
            Content::Fill(Rgb(5, 5, 5)),
            Instant::now(),
        )
        .unwrap();

        h.poison(); // kernel thread panics; governor schedules a rebirth (~750ms)

        // Poll until the reborn kernel answers again (the queued commands
        // during the sleep are served after rebirth — this recv just blocks).
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let surfaces = h.surfaces();
            if !surfaces.is_empty() {
                assert_eq!(surfaces[0].key, "kbd");
                break;
            }
            assert!(Instant::now() < deadline, "kernel did not rebirth in time");
            thread::sleep(Duration::from_millis(50));
        }
        // Surfaces survived (the seed); the session's claim did NOT (leases
        // are never reborn — sessions must re-claim).
        let frame = h.resolve("kbd", Instant::now()).expect("surface exists after rebirth");
        assert_eq!(frame, vec![None]);
    }

    #[test]
    fn source_ids_are_not_reused_after_rebirth() {
        // A surviving session keeps its old owner id across a kernel fault. The
        // reborn kernel must NOT hand that same id to a new caller, or an
        // owner-wide `release_owner` from either would drop both.
        let host = Host::spawn();
        let mut h = host.handle();
        let a = h.next_source();
        let b = h.next_source();
        assert!(b.0 > a.0, "ids increase within one incarnation");

        h.poison(); // kernel thread panics; governor schedules a rebirth

        // This request queues during the rebirth sleep and is served by the
        // reborn kernel — so its answer reflects the post-rebirth counter.
        let c = h.next_source();
        assert_ne!(c, SourceId(u64::MAX), "kernel answered after rebirth");
        assert!(
            c.0 > b.0,
            "reused a source id across rebirth: {c:?} not above {b:?}"
        );
    }

    // ── TASK 2 (second half): claim fight racing a real kernel fault ───────
    //
    // `Cmd::Poison`/`HostHandle::poison` are `#[cfg(test)]`-gated INSIDE this
    // crate, so an EXTERNAL integration-test binary (like
    // `tests/claim_fights.rs`, which links the lib without `--cfg test`)
    // cannot see them — this scenario has to live here instead. The
    // no-poisoning concurrent claim fight lives in `tests/claim_fights.rs`,
    // which only needs the public `HostApi`/`HostHandle` surface.

    #[test]
    fn claim_fight_survives_poisoning_mid_fight() {
        let host = Host::spawn();
        let mut setup = host.handle();
        setup.declare(SurfaceInfo::grid("kbd", "Board", SurfaceKind::Keyboard, 1, 1));

        const THREADS: usize = 4;
        const ITERS: usize = 30;
        let workers: Vec<_> = (0..THREADS)
            .map(|t| {
                let mut h = host.handle();
                thread::Builder::new()
                    .name(format!("poison-fight-{t}"))
                    .spawn(move || {
                        for i in 0..ITERS {
                            let owner = h.next_source();
                            let lease = if i % 2 == 0 {
                                LeaseSpec::Ttl(Duration::from_millis(10))
                            } else {
                                LeaseSpec::Pinned
                            };
                            // A claim landing during the crash/rebirth window
                            // honestly returns `None` (channel gone or kernel
                            // mid-fault) — never panics, never hangs, bounded
                            // by the governor's own restart contract.
                            if let Some(id) = h.claim(
                                "kbd",
                                owner,
                                band::SESSION,
                                lease,
                                Content::Fill(Rgb(t as u8, i as u8, 0)),
                                Instant::now(),
                            ) {
                                h.refresh(id, Instant::now());
                            }
                        }
                    })
                    .expect("spawn poison-fight thread")
            })
            .collect();

        // Poison the kernel twice, spaced enough to be isolated faults each
        // (the escalation-burst composed contract is TASK 4's job, not this
        // one's — this test is about surviving a fault WHILE claims race it).
        let poisoner = host.handle();
        poisoner.poison();
        thread::sleep(Duration::from_millis(120));
        poisoner.poison();

        for w in workers {
            assert!(
                w.join().is_ok(),
                "a claim-fight thread must never panic across a kernel fault"
            );
        }

        // Bound every recv the same way `kernel_fault_rebirths_...` above
        // does: poll for the reborn kernel rather than trusting a single
        // blocking call.
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut h = host.handle();
        loop {
            let surfaces = h.surfaces();
            if !surfaces.is_empty() {
                break;
            }
            assert!(Instant::now() < deadline, "kernel never came back after the poisoning burst");
            thread::sleep(Duration::from_millis(50));
        }
        // Re-claim after rebirth must succeed — the documented lease
        // contract (surfaces reborn, sessions must re-claim).
        let owner = h.next_source();
        let now = Instant::now();
        assert!(
            h.claim("kbd", owner, band::SESSION, LeaseSpec::Pinned, Content::Fill(Rgb(9, 9, 9)), now)
                .is_some(),
            "a fresh claim after rebirth must succeed"
        );
    }

    // ── TASK 4: governor × shell composed contract under a crash BURST ─────

    #[test]
    fn crash_burst_never_hangs_and_the_composed_governor_contract_holds() {
        // `kernel_fault_rebirths_with_surfaces_but_without_claims` proves ONE
        // rebirth. This drives a BURST — repeated poisoning in quick
        // succession — to observe the ACTUAL composed contract. Per this
        // module's own docs: "isolated faults restart in under a second, a
        // fault storm escalates and the shell gives up loudly rather than
        // spinning" — `Verdict::Escalate` makes `run` `break`, so the actor
        // thread exits FOR GOOD (see `run`'s match on `governor.on_crash`).
        // There is no separate "give up after N tries, then restart clean"
        // tier in this composition: once escalated, the Host stays
        // permanently dead and every `HostHandle` call degrades honestly.
        // This test's whole point is proving that degrade is FAST, not a
        // hang, whichever branch (kept-absorbing vs escalated) the burst
        // actually lands in on this machine.
        let host = Host::spawn();
        let mut setup = host.handle();
        setup.declare(SurfaceInfo::grid("kbd", "Board", SurfaceKind::Keyboard, 1, 1));

        let poison_handle = host.handle();
        let mut probe_handle = host.handle();
        let scenario = crate::worker::spawn_named("t-crash-burst", move || {
            const BURST: usize = 6;
            for _ in 0..BURST {
                poison_handle.poison();
            }
            // Poll for a settled reading, exiting as soon as two consecutive
            // probes agree (early-exit keeps the common case fast); bounded
            // overall so a genuine non-convergence still finishes promptly.
            let ceiling = Instant::now() + Duration::from_secs(8);
            let mut prev: Option<bool> = None;
            let settled;
            loop {
                let alive = !probe_handle.surfaces().is_empty();
                if prev == Some(alive) {
                    settled = alive;
                    break;
                }
                prev = Some(alive);
                if Instant::now() >= ceiling {
                    settled = alive;
                    break;
                }
                thread::sleep(Duration::from_millis(150));
            }
            settled
        })
        .expect("spawn crash-burst scenario thread");

        let alive_at_rest =
            crate::worker::join_bounded(scenario, Duration::from_secs(15), "t-crash-burst").expect(
                "the burst-and-settle scenario must finish within its bound — a hang here would \
                 mean the governor/shell composition can spin-restart or block forever, contrary \
                 to the documented 'gives up loudly rather than spinning' contract",
            );

        // Whichever branch the burst landed in, a FRESH probe right now must
        // be fast and consistent with that resting state — never itself a
        // hang, and (if escalated) never a fluke straggler answer.
        let mut h = host.handle();
        let probed = h.surfaces();
        if alive_at_rest {
            assert_eq!(
                probed.first().map(|s| s.key.as_str()),
                Some("kbd"),
                "still-alive branch: the seed must still be served"
            );
        } else {
            assert!(probed.is_empty(), "escalated host must stay dead, not intermittently answer");
            thread::sleep(Duration::from_millis(200));
            assert!(h.surfaces().is_empty(), "escalated host must not un-escalate on its own");
        }
    }
}
