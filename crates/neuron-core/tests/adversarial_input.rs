// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

//! Hostile input for everything that reads devices it has never seen: report descriptors with any
//! ranges, reports of any bytes, layout files with any numbers, sticks that report NaN, clocks that
//! jump, badge files with anything in them. Nothing here may panic, and every value that reaches a
//! motor, a bind or the screen must stay in its range.

use neuron::analog::{Device, Field, AXIS_NEG_PAGE, AXIS_POS_PAGE, HAT_PAGE};
use neuron::controls::{control_label, ControlEvent, ControlRef, HoldEdges, InputEdge, Stream};
use neuron::haptics::Rumble;
use neuron::knob::{mix, quantise, Cue, Knob};
use neuron::layout::{bits, AxisBits, ButtonBit, Layout};
use neuron::radial::StickAim;
use neuron::registry::CanonicalPid;
use proptest::prelude::*;

fn any_field() -> impl Strategy<Value = Field> {
    (
        prop_oneof![Just(0x01u16), Just(0x02), Just(0x05), Just(0xFE), any::<u16>()],
        prop_oneof![Just(0x30u16), Just(0x31), Just(0x32), Just(0x33), Just(0x34), Just(0x35), Just(0x39), any::<u16>()],
        any::<i32>(),
        any::<i32>(),
    )
        .prop_map(|(page, usage, a, b)| Field { page, usage, logical_min: a.min(b), logical_max: a.max(b) })
}

fn finite_unit(r: Rumble) -> bool {
    [r.low, r.high, r.left_trigger, r.right_trigger].iter().all(|v| v.is_finite() && (0.0..=1.0).contains(v))
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 512, ..ProptestConfig::default() })]

    /// Any descriptor, any values, any clock (including one that runs backwards): the analog model
    /// never panics, sticks stay on the unit square, and it only ever reports its own pages.
    #[test]
    fn analog_model_survives_any_descriptor_and_stream(
        fields in prop::collection::vec(any_field(), 1..6),
        frames in prop::collection::vec((prop::collection::vec(any::<i32>(), 6), any::<u64>()), 1..40),
    ) {
        let mut d = Device::default();
        for (vals, now) in frames {
            let values: Vec<(Field, i32)> = fields.iter().copied().zip(vals).collect();
            for (page, _) in d.observe(&values, now) {
                prop_assert!(matches!(page, AXIS_POS_PAGE | AXIS_NEG_PAGE | HAT_PAGE));
            }
            for s in d.sticks() {
                prop_assert!(s.x.is_finite() && s.y.is_finite());
                prop_assert!((-1.0..=1.0).contains(&s.x) && (-1.0..=1.0).contains(&s.y));
            }
        }
    }

    /// A bit field anywhere, of any width up to 32, in a report of any length: read or refuse.
    #[test]
    fn bit_fields_never_read_past_the_report(report in prop::collection::vec(any::<u8>(), 0..80), bit in any::<usize>(), len in 0u8..=32) {
        if let Some(v) = bits(&report, bit, len) {
            prop_assert!(len == 32 || u64::from(v) < (1u64 << len));
        }
    }

    /// A layout with any (valid-shaped) bit positions and ranges, over any report: axes land in
    /// -1..1, triggers in 0..1, or the report is refused.
    #[test]
    fn layouts_decode_any_report_into_range(
        report in prop::collection::vec(any::<u8>(), 0..64),
        buttons in prop::collection::vec((any::<usize>(), any::<u8>(), 0usize..4), 0..8),
        axes in prop::collection::vec((0usize..600, 1u8..=32, any::<u32>(), any::<u32>(), any::<bool>(), 0usize..6), 0..6),
    ) {
        let button_names = ["south", "left_trigger", "home", "dpad_up"];
        let axis_names = ["left_x", "left_y", "right_x", "right_y", "left_trigger", "right_trigger"];
        let layout = Layout {
            name: "fuzz".into(),
            vendor_id: 1,
            product_ids: vec![2],
            report_id: report.first().copied().unwrap_or(0),
            buttons: buttons.into_iter().map(|(byte, mask, n)| ButtonBit { byte, mask, pad: button_names[n].into() }).collect(),
            axes: axes
                .into_iter()
                .filter(|(_, _, a, b, _, _)| a != b)
                .map(|(bit, bits, a, b, invert, n)| AxisBits { pad: axis_names[n].into(), bit, bits, min: a.min(b), max: a.max(b), invert })
                .collect(),
            init: Vec::new(),
        };
        if let Some(pad) = layout.decode(&report) {
            for v in [pad.left_x, pad.left_y, pad.right_x, pad.right_y] {
                prop_assert!(v.is_finite() && (-1.0..=1.0).contains(&v), "axis {v}");
            }
            for v in [pad.left_trigger, pad.right_trigger] {
                prop_assert!(v.is_finite() && (0.0..=1.0).contains(&v), "trigger {v}");
            }
        }
    }

    /// Stick samples that are NaN, infinite or enormous, at any time and on any wheel: the knob's
    /// field is always a finite strength in 0..1, and every cue it starts plays out in range.
    #[test]
    fn the_knob_never_drives_a_motor_out_of_range(
        samples in prop::collection::vec((any::<f64>(), any::<f64>(), any::<f64>()), 1..60),
        sectors in 0usize..64,
    ) {
        let mut k = Knob::default();
        let mut cues = Vec::new();
        for (x, y, t) in samples {
            let field = k.feel(x, y, t, sectors, |w| w % 2 == 0, &mut cues);
            prop_assert!(finite_unit(field), "{field:?} from ({x}, {y}, {t})");
        }
        for cue in cues {
            for ms in [0.0f32, 1.0, 9.9, 10.0, 49.0, 50.0, 59.9, 200.0, 1e6] {
                if let Some(r) = cue.at(ms) {
                    prop_assert!(finite_unit(mix(r, [])), "{cue:?} at {ms}");
                }
            }
        }
    }

    /// Mixing any strengths with any cues at any age clamps; quantising any float is a valid level.
    #[test]
    fn mixing_and_quantising_are_total(low in any::<f32>(), high in any::<f32>(), ms in any::<f32>()) {
        let r = mix(Rumble { low, high, left_trigger: high, right_trigger: low }, [(Cue::Fire, ms.abs()), (Cue::Rim, ms.abs())]);
        for v in [r.low, r.high, r.left_trigger, r.right_trigger] {
            prop_assert!(v.is_nan() || (0.0..=1.0).contains(&v));
        }
        for level in quantise(r) {
            prop_assert!(level <= 64);
        }
    }

    /// A stick aim fed garbage never produces a stroke that isn't finite.
    #[test]
    fn stick_aim_strokes_are_finite(samples in prop::collection::vec((any::<f64>(), any::<f64>(), any::<f64>()), 1..40), deadzone in 0.0f64..500.0) {
        let mut a = StickAim::default();
        for (x, y, t) in samples {
            a.observe(x, y, t);
        }
        if let Some(path) = a.path(deadzone) {
            prop_assert!(path.iter().all(|c| c.re.is_finite() && c.im.is_finite()), "{path:?}");
        }
    }

    /// Every (page, usage) a device could ever report has a name, and a pid-scoped control's label
    /// is never empty.
    #[test]
    fn every_control_has_a_name(page in any::<u16>(), usage in any::<u16>(), pid in any::<u16>()) {
        prop_assert!(!control_label(page, usage).is_empty());
        let c = ControlRef { page, usage, pid: Some(CanonicalPid::of(pid)) };
        prop_assert!(!c.name().is_empty() && c.label().len() > c.name().len());
    }

    /// Any sequence of pad snapshots, then silence (the device left): every control that went down
    /// comes back up exactly once, so nothing stays held.
    #[test]
    fn a_device_that_goes_silent_releases_everything(
        frames in prop::collection::vec(prop::collection::vec((0xFE20u16..0xFE22, 0u16..8), 0..5), 1..30),
    ) {
        let mut edges = HoldEdges::new();
        let pid = Some(CanonicalPid::of(0x543A));
        let mut held = std::collections::BTreeSet::new();
        let ev = |hits: Vec<(u16, u16)>| ControlEvent { pid, stream: Stream::RawInput, hits, raw: Vec::new() };
        for hits in frames.into_iter().chain(std::iter::once(Vec::new())) {
            for e in edges.edges(&ev(hits)) {
                match e {
                    InputEdge::Down(t) => prop_assert!(held.insert(format!("{t:?}")), "double down {t:?}"),
                    InputEdge::Up(t) => prop_assert!(held.remove(&format!("{t:?}")), "up without down {t:?}"),
                }
            }
        }
        prop_assert!(held.is_empty(), "still held: {held:?}");
    }

    /// The descriptor-honesty monitor takes any stream at any clock without panicking.
    #[test]
    fn honesty_monitor_is_total(frames in prop::collection::vec((prop::collection::vec((any::<u16>(), any::<u16>()), 0..8), any::<u64>()), 0..50)) {
        let mut m = neuron::honesty::Monitor::default();
        for (hits, now) in frames {
            m.observe(&hits, now);
        }
    }
}

/// A badge file written by anyone, or by a crashed editor, never stops a device from being named;
/// any name the user types survives a save and a reload intact, trimmed of edge whitespace.
#[test]
fn badge_files_with_anything_in_them() {
    let dir = std::env::temp_dir().join(format!("neuron_badge_fuzz_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    // SAFETY-free: this is the only test in this binary that reads the run root.
    unsafe { std::env::set_var("NEURON_RUN_DIR", &dir) };
    let pid = CanonicalPid::of(0xB0B0);
    for junk in [
        "",
        "\u{0}\u{1}garbage",
        "[device]\nb0b0 = 7",
        "[device.b0b0]\nemblem = 42\nname = []",
        "[device.zzzz]\nname = \"not hex\"",
        "[device.b0b0]\nemblem = \"spaceship\"",
        &"[".repeat(10_000),
    ] {
        std::fs::write(dir.join("badges.toml"), junk).unwrap();
        // A fresh read of the file: point the run root away and back.
        unsafe { std::env::set_var("NEURON_RUN_DIR", dir.join("elsewhere")) };
        let _ = neuron::badge::of(pid);
        unsafe { std::env::set_var("NEURON_RUN_DIR", &dir) };
        let b = neuron::badge::of(pid);
        assert!(!b.name.is_empty(), "a device always has a name ({junk:?})");
    }
    let names = ["couch pad", "  padded  ", "quote \" and \\ back", "ünïcødé 🎮", "new\nline", "[device.x]", &"x".repeat(4096)];
    for name in names {
        neuron::badge::set(pid, Some(neuron::badge::Emblem::Keypad), Some(name)).unwrap();
        unsafe { std::env::set_var("NEURON_RUN_DIR", dir.join("elsewhere")) };
        let _ = neuron::badge::of(pid);
        unsafe { std::env::set_var("NEURON_RUN_DIR", &dir) };
        let b = neuron::badge::of(pid);
        assert_eq!(b.name, name.trim(), "round trip of {name:?}");
        assert_eq!(b.emblem, neuron::badge::Emblem::Keypad);
    }
    neuron::badge::set(pid, None, Some("   ")).unwrap();
    assert!(!neuron::badge::of(pid).custom_name, "a blank name is automatic, not a blank badge");
    let _ = std::fs::remove_dir_all(&dir);
}

/// The knob's renderer and the haptics sinks under contention from many threads: no deadlock, no
/// panic, and the motors end at rest once everything stops.
#[test]
fn knob_and_haptics_under_contention() {
    use std::sync::{Arc, Mutex};
    struct Last(Arc<Mutex<Vec<Rumble>>>);
    impl neuron::haptics::Sink for Last {
        fn owns(&self, device: &str) -> bool {
            device == "adversarial#pad"
        }
        fn set(&self, device: &str, rumble: Rumble) -> bool {
            if device == "adversarial#pad" {
                self.0.lock().unwrap().push(rumble);
                return true;
            }
            false
        }
    }
    let log = Arc::new(Mutex::new(Vec::new()));
    neuron::haptics::register(Box::new(Last(log.clone())));
    let threads: Vec<_> = (0..8)
        .map(|n| {
            std::thread::spawn(move || {
                for i in 0..300 {
                    let a = f64::from(i * (n + 1)) * 0.05;
                    neuron::knob::aim("adversarial#pad", a.sin(), -a.cos(), 8, &[0, 3, 0, 2]);
                    match i % 97 {
                        0 => neuron::knob::fire(),
                        50 => neuron::knob::cancel(),
                        _ => {}
                    }
                    if i % 41 == 0 {
                        neuron::haptics::pulse("adversarial#pad", Rumble { low: 0.3, ..Rumble::OFF }, 5);
                    }
                }
            })
        })
        .collect();
    for t in threads {
        t.join().unwrap();
    }
    neuron::knob::cancel();
    // Cues last at most ~320 ms and a stale stick fades in 60 ms; allow generous slack.
    std::thread::sleep(std::time::Duration::from_millis(900));
    let log = log.lock().unwrap();
    assert!(!log.is_empty(), "the renderer drove the sink");
    assert!(log.iter().all(|r| finite_unit(*r)));
    assert_eq!(log.last().copied(), Some(Rumble::OFF), "motors end at rest");
}

/// One device, several collections: a click on its pointer must not read as the release of a key
/// held on its keyboard collection (a HyperShift layer held on a side key would drop).
#[test]
fn one_devices_collections_never_release_each_other() {
    let naga = Some(CanonicalPid::of(0x00A7));
    let mut edges = HoldEdges::new();
    let ev = |stream, hits: Vec<(u16, u16)>| ControlEvent { pid: naga, stream, hits, raw: Vec::new() };
    assert_eq!(edges.edges(&ev(Stream::Keyboard, vec![(0x07, 0x1E)])).len(), 1);
    let click = edges.edges(&ev(Stream::Pointer, vec![(0x09, 1)]));
    assert!(click.iter().all(|e| matches!(e, InputEdge::Down(_))), "{click:?}");
    let consumer = neuron::controls::Stream::Collection(neuron::controls::collection_id(r"\?\HID#VID_1532&PID_00A7&MI_02&Col02#x"));
    let knob = edges.edges(&ev(consumer, Vec::new()));
    assert!(knob.is_empty(), "an idle consumer collection releases nothing: {knob:?}");
    let up = edges.edges(&ev(Stream::Keyboard, Vec::new()));
    assert_eq!(up.len(), 1, "only the key's own release releases it");
}
