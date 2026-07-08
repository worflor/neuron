//! The host shell — the kernel as an immortal actor.
//!
//! One thread owns the [`Kernel`]. Everyone else holds a [`HostHandle`]
//! (cloneable) and speaks [`HostApi`] over a channel — there is no shared
//! mutex on kernel state, so there is nothing to poison; a panicking caller
//! can never corrupt the kernel, and a kernel fault can never deadlock a
//! caller (their reply sender drops, they get a default, they carry on).
//!
//! Immortality is the §6 design, composed from the pieces already proven in
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
//!   SHM, OpenRGB) already carries its own tested reassert/recovery path —
//!   a REST heartbeat, an OpenRGB idle-tick reassert, an SHM refresh, an
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
    pub fn spawn() -> Host {
        let (tx, rx) = channel();
        let thread = thread::Builder::new()
            .name("neuron-host-kernel".into())
            .spawn(move || run(rx))
            .expect("spawn kernel actor");
        Host { tx, thread: Some(thread) }
    }

    pub fn handle(&self) -> HostHandle {
        HostHandle { tx: self.tx.clone() }
    }
}

impl Drop for Host {
    fn drop(&mut self) {
        let _ = self.tx.send(Cmd::Shutdown);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
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
        rrx.recv().ok()
    }

    /// Subscribe to bus signals by prefix (see [`crate::bus::Bus::subscribe`]).
    /// The receiver closes on kernel rebirth — resubscribe on disconnect.
    pub fn subscribe(&self, prefix: &str) -> Option<Receiver<Signal>> {
        self.request(|reply| Cmd::Subscribe { prefix: prefix.into(), reply })
    }

    /// The alive claims on a surface, topmost first, with adapter-provided
    /// names attached — the "who is controlling this board" readout. Empty if
    /// the kernel is gone or the surface unknown.
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
}
