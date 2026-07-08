//! The capstone: every piece composed, end to end.
//!
//! An OpenRGB client's raw wire bytes flow through the adapter, over the
//! actor channel into the kernel, the arbiter resolves ownership, and the
//! paced writer delivers frames to a (mock) device sink — while the user's
//! base lighting waits underneath and returns the instant the client
//! disconnects. This is the R&D doc's §5.1 story running as real threads.

use std::time::{Duration, Instant};

use neuron_host::adapters::openrgb::{ids, packet, OrgbConn};
use neuron_host::api::{HostApi, LeaseSpec, SurfaceInfo, SurfaceKind};
use neuron_host::arbiter::{band, Content, Rgb};
use neuron_host::paint::PaintPolicy;
use neuron_host::shell::Host;
use neuron_host::writer::{MockSink, Writer};

/// Poll the sink until its latest frame satisfies `pred` (or time out).
fn eventually(sink: &MockSink, timeout: Duration, pred: impl Fn(&[Option<Rgb>]) -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if let Some(last) = sink.last() {
            if pred(&last) {
                return true;
            }
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    false
}

#[test]
fn wire_bytes_to_device_frames_with_clean_fallback() {
    let host = Host::spawn();

    // The app declares a keyboard and its configured base lighting (green).
    let mut h = host.handle();
    h.declare(SurfaceInfo::grid("kbd", "Board", SurfaceKind::Keyboard, 1, 4));
    let base = h.next_source();
    h.claim(
        "kbd",
        base,
        band::BASE,
        LeaseSpec::Pinned,
        Content::Fill(Rgb(0, 255, 0)),
        Instant::now(),
    )
    .expect("base claim");

    // The single writer for this device, feeding a mock sink at 50 fps (the
    // factory runs on the writer thread — where a real HID sink is born).
    let sink = MockSink::new();
    let writer_sink = sink.clone();
    let _writer = Writer::spawn(host.handle(), "kbd", 50, move || writer_sink);

    assert!(
        eventually(&sink, Duration::from_secs(2), |f| f
            .iter()
            .all(|c| *c == Some(Rgb(0, 255, 0)))),
        "writer must deliver the base frame"
    );

    // An OpenRGB client connects (its own handle = its own channel into the
    // one kernel) and paints the board red over the wire.
    let mut client_side = host.handle();
    // Opaque policy: paint shows exactly as sent, no fade — so the writer records
    // only all-green or all-red frames (the atomic-resolution assertion below).
    let mut conn = OrgbConn::new(&mut client_side, PaintPolicy::opaque());
    let mut payload = Vec::new();
    payload.extend_from_slice(&0u32.to_le_bytes());
    payload.extend_from_slice(&4u16.to_le_bytes());
    for _ in 0..4 {
        payload.extend_from_slice(&[255, 0, 0, 0]);
    }
    let reply = conn.feed(&packet(0, ids::UPDATELEDS, &payload), &mut client_side, Instant::now());
    assert!(reply.is_empty());

    assert!(
        eventually(&sink, Duration::from_secs(2), |f| f
            .iter()
            .all(|c| *c == Some(Rgb(255, 0, 0)))),
        "client's paint must reach the device sink"
    );

    // The client disconnects: its footprint releases, base shows through —
    // no flicker window, no stuck lighting, nobody had to remember anything.
    conn.disconnected(&mut client_side);
    assert!(
        eventually(&sink, Duration::from_secs(2), |f| f
            .iter()
            .all(|c| *c == Some(Rgb(0, 255, 0)))),
        "base lighting must return after disconnect"
    );

    // And the writer never wrote a junk intermediate: every recorded frame is
    // all-green or all-red (resolution is atomic per frame — no tearing).
    for frame in sink.frames() {
        let all_green = frame.iter().all(|c| *c == Some(Rgb(0, 255, 0)));
        let all_red = frame.iter().all(|c| *c == Some(Rgb(255, 0, 0)));
        assert!(all_green || all_red, "torn frame observed: {frame:?}");
    }
}
