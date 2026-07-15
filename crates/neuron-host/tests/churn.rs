//! Lifecycle churn -- the unit-level sibling of the whole-app resident BUDGET lane
//! (`neuron-testkit::budget`). Every thread-owning type in this crate (`OrgbServer`,
//! `ChromaHttpServer`, `ObsConnection`, `Writer`) grew a bounded, join-on-Drop teardown so a
//! wedged sink/socket leaks a thread instead of hanging the process (see the
//! `*_drop_is_bounded_*` tests in `net.rs`/`writer.rs`). Bounded is not the same as CLEAN: a
//! drop that always returns could still leave a handle or a thread behind every single cycle.
//! These tests construct-use briefly-drop each type dozens of times and assert the process's
//! own thread/handle census returns to baseline -- the thing a single hung-drop test can't see.
//!
//! House rules: every baseline is taken AFTER one untimed warmup cycle (first-use lazy init --
//! CRT/loader/DNS caches -- must not read as a leak), and every post-churn census goes through
//! `census::settle` before comparing (thread teardown is asynchronous; asserting immediately
//! after the last drop is exactly the flake this exists to avoid).
//!
//! Windows-only: the census (`neuron_testkit::census`) this whole file is built around is a
//! Win32 ToolHelp/handle-count instrument (see that module's doc comment) and is not compiled
//! on other platforms.

#![cfg(windows)]

use std::net::{TcpListener, TcpStream};
use std::sync::{Mutex, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use neuron_host::api::{HostApi, LeaseSpec, SurfaceInfo, SurfaceKind};
use neuron_host::arbiter::{band, Content, Rgb};
use neuron_host::net::{ChromaHttpServer, ObsConnection, OrgbServer};
use neuron_host::shell::Host;
use neuron_host::writer::{MockSink, Writer};
use neuron_testkit::census::{assert_converges, settle, CensusTolerance};

/// Generous but bounded -- every test here is a churn loop over cheap, local, loopback-only
/// work; this is a backstop against a genuine hang, not a tuned expectation.
const SETTLE_DEADLINE: Duration = Duration::from_secs(2);

/// `cargo test` runs every test FUNCTION in this binary concurrently by default, but a census
/// reads whole-PROCESS thread/handle counts -- two of these tests racing would each see the
/// other's churn as unexplained drift. This file is its OWN test binary (Cargo gives every
/// `tests/*.rs` file a separate process), so serializing just these five tests against each
/// other is sufficient for full isolation, unlike the shared-lib-binary case in
/// `neuron-testkit::census`'s own unit tests.
static CENSUS_LOCK: Mutex<()> = Mutex::new(());

/// A dead loopback address: bind an ephemeral listener, read its address, then release it. On a
/// consumer machine loopback ephemeral ports are not reused fast enough to collide within a
/// single test run, so `addr` stays refused (`ConnectionRefused`) for every churn cycle below --
/// exactly the "toward a dead port" shape `ObsConnection`'s retry loop must tear down cleanly
/// from.
fn dead_addr() -> String {
    let l = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral to mint a dead addr");
    let addr = l.local_addr().unwrap().to_string();
    drop(l);
    addr
}

/// One TCP connect-then-disconnect against a bound server, with a short grace so the server's
/// accept loop (nonblocking, polled every <=50ms -- see `net.rs`) often notices the connection
/// and spins up (and then tears down) a per-connection thread before the caller moves on -- that
/// spin-up/tear-down is the thing under test, not just the bind. 20ms is NOT long enough to
/// guarantee every one of the (up to) 50 churn cycles below hits that window (the accept loop's
/// own poll cadence can push a given cycle past it), but the accept thread's FIRST `accept()`
/// call -- before it ever sleeps -- typically wins the race against our connect on loopback, so
/// most cycles land inside it anyway; the alternative (a fixed 60ms wait matching the full poll
/// interval) would blow this now-serialized suite's time budget across five tests.
fn poke_tcp(addr: std::net::SocketAddr) {
    if let Ok(s) = TcpStream::connect(addr) {
        drop(s);
    }
    thread::sleep(Duration::from_millis(20));
}

#[test]
fn orgb_server_churn_leaves_no_threads_or_handles() {
    let _guard = CENSUS_LOCK.lock().unwrap_or_else(PoisonError::into_inner);
    let host = Host::spawn();

    // Warmup: first bind/accept pays for anything lazily initialized (loader, CRT, DNS/loopback
    // route caches) that a baseline taken before it would wrongly count as "leaked" later.
    poke_tcp(OrgbServer::bind("127.0.0.1:0", host.handle()).expect("warmup bind").addr());

    let baseline = settle(SETTLE_DEADLINE);
    for _ in 0..50 {
        let server = OrgbServer::bind("127.0.0.1:0", host.handle()).expect("bind ephemeral");
        poke_tcp(server.addr());
        drop(server);
    }
    let post = settle(SETTLE_DEADLINE);
    assert_converges(baseline, post, CensusTolerance::default(), "OrgbServer x50");
}

#[test]
fn chroma_http_server_churn_leaves_no_threads_or_handles() {
    let _guard = CENSUS_LOCK.lock().unwrap_or_else(PoisonError::into_inner);
    let host = Host::spawn();

    poke_tcp(ChromaHttpServer::bind("127.0.0.1:0", host.handle()).expect("warmup bind").addr());

    let baseline = settle(SETTLE_DEADLINE);
    for _ in 0..50 {
        let server = ChromaHttpServer::bind("127.0.0.1:0", host.handle()).expect("bind ephemeral");
        poke_tcp(server.addr());
        drop(server);
    }
    let post = settle(SETTLE_DEADLINE);
    assert_converges(baseline, post, CensusTolerance::default(), "ChromaHttpServer x50");
}

/// Construct a `Writer` on `host`/`surface`, wait for at least one frame to actually land in a
/// fresh `MockSink`, then drop it. `host` must already have `surface` declared and claimed --
/// otherwise `resolve` never yields a frame and this would hang waiting for one that can't come.
fn spawn_use_drop_writer(host: &Host, surface: &str) {
    let sink = MockSink::new();
    let probe = sink.clone();
    let writer = Writer::spawn(host.handle(), surface, 30, move || sink);
    let deadline = Instant::now() + Duration::from_millis(500);
    while probe.last().is_none() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(5));
    }
    assert!(probe.last().is_some(), "writer must deliver at least one frame before teardown");
    drop(writer);
}

#[test]
fn writer_churn_leaves_no_threads_or_handles() {
    let _guard = CENSUS_LOCK.lock().unwrap_or_else(PoisonError::into_inner);
    let host = Host::spawn();
    let mut h = host.handle();
    h.declare(SurfaceInfo::grid("kbd", "Board", SurfaceKind::Keyboard, 1, 1));
    let owner = h.next_source();
    h.claim(
        "kbd",
        owner,
        band::BASE,
        LeaseSpec::Pinned,
        Content::Fill(Rgb(7, 7, 7)),
        Instant::now(),
    )
    .expect("base claim");

    spawn_use_drop_writer(&host, "kbd"); // warmup

    let baseline = settle(SETTLE_DEADLINE);
    for _ in 0..50 {
        spawn_use_drop_writer(&host, "kbd");
    }
    let post = settle(SETTLE_DEADLINE);
    assert_converges(baseline, post, CensusTolerance::default(), "Writer x50");
}

/// Construct an `ObsConnection` aimed at a dead port and immediately drop it. The connection's
/// retry loop starts a `WsStream::connect` attempt that fails fast (loopback refuses instantly),
/// falls into its backoff sleep, and must still tear down within its documented drop deadline --
/// this is that promise exercised across many lifecycles, not just one (`net.rs`'s
/// `obs_connection_drop_is_bounded_*` test only proves ONE cycle is bounded).
fn spawn_use_drop_obs(host: &Host, dead: &str) {
    let conn = ObsConnection::start(dead, "", host.handle());
    thread::sleep(Duration::from_millis(10)); // let the retry loop actually start dialing
    drop(conn);
}

#[test]
fn obs_connection_churn_leaves_no_threads_or_handles() {
    let _guard = CENSUS_LOCK.lock().unwrap_or_else(PoisonError::into_inner);
    let host = Host::spawn();
    let dead = dead_addr();

    spawn_use_drop_obs(&host, &dead); // warmup

    let baseline = settle(SETTLE_DEADLINE);
    // Fewer cycles than the servers: each carries its own connect attempt plus a backoff-sleep
    // window inside the drop bound, so this is sized down to keep the suite well inside its 15s
    // budget rather than sized against a real risk of leaking.
    for _ in 0..15 {
        spawn_use_drop_obs(&host, &dead);
    }
    let post = settle(SETTLE_DEADLINE);
    assert_converges(baseline, post, CensusTolerance::default(), "ObsConnection x15");
}

/// All four thread-owning types churned TOGETHER -- an interaction leak (one type's teardown
/// stealing time from another's stop-check cadence, or a shared dependency like the kernel
/// actor thread growing per round) would not necessarily show up when each type is churned in
/// isolation above.
#[test]
fn combined_churn_of_every_thread_owning_type_leaves_no_threads_or_handles() {
    let _guard = CENSUS_LOCK.lock().unwrap_or_else(PoisonError::into_inner);
    let host = Host::spawn();
    let mut h = host.handle();
    h.declare(SurfaceInfo::grid("kbd", "Board", SurfaceKind::Keyboard, 1, 1));
    let owner = h.next_source();
    h.claim(
        "kbd",
        owner,
        band::BASE,
        LeaseSpec::Pinned,
        Content::Fill(Rgb(3, 3, 3)),
        Instant::now(),
    )
    .expect("base claim");
    let dead = dead_addr();

    let one_cycle_of_everything = || {
        let orgb = OrgbServer::bind("127.0.0.1:0", host.handle()).expect("bind orgb");
        poke_tcp(orgb.addr());
        let chroma = ChromaHttpServer::bind("127.0.0.1:0", host.handle()).expect("bind chroma");
        poke_tcp(chroma.addr());
        spawn_use_drop_writer(&host, "kbd");
        spawn_use_drop_obs(&host, &dead);
    };
    one_cycle_of_everything(); // warmup: pay for lazy first-use init once, before the baseline

    let baseline = settle(SETTLE_DEADLINE);
    for _ in 0..10 {
        one_cycle_of_everything();
    }
    let post = settle(SETTLE_DEADLINE);
    assert_converges(
        baseline,
        post,
        CensusTolerance::default(),
        "OrgbServer+ChromaHttpServer+Writer+ObsConnection x10",
    );
}
