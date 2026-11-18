// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo Research Components Exception 1.0.
// See ../../../LICENSE.md.

//! Keyboard conformance: every Razer keyboard family in the atlas, under every firmware
//! assumption the sources leave open, through the production path a user's board takes.
//!
//! Each case builds a phantom unit from the atlas entry (its matrix size, its lighting tx, its
//! LED and control collection, plus a decoy collection that answers identity getters but no
//! lighting), lets `synth::adopt_unknown` adopt it, reloads the registry, opens the device the
//! way the app does, then sets a static colour, a brightness and a custom frame and asserts what
//! the phantom's LEDs show. A pass means neuron reaches a board that behaves this way; it is not
//! evidence that a real board does. The atlas-blind cases repeat a family under a PID the atlas
//! does not know, so discovery is tested without its priors.

use anyhow::{bail, ensure, Context, Result};
use neuron::{
    atlas::{self, Class, Keyboard},
    capability::{self, Store},
    device::Device,
    lighting::{Effect, Lights, Rgb},
    registry::Registry,
    runroot::RunDirPin,
    synth,
    transport::{
        self,
        firmware::{Behaviour, Board, Family, FirmwareBackend, Refusal, Role, Scope, Shown, Unit},
    },
};
use std::{
    path::PathBuf,
    sync::{Arc, PoisonError},
    time::{SystemTime, UNIX_EPOCH},
};

struct IsolatedRunRoot {
    path: PathBuf,
    _pin: RunDirPin,
}

impl IsolatedRunRoot {
    fn new(tag: &str) -> Result<Self> {
        let nonce = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_nanos();
        let path = std::env::temp_dir().join(format!("neuron_kbd_conf_{}_{tag}_{nonce}", std::process::id()));
        std::fs::create_dir(&path)?;
        let pin = RunDirPin::to(&path);
        Ok(Self { path, _pin: pin })
    }
}

impl Drop for IsolatedRunRoot {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// One firmware assumption the sources leave open.
#[derive(Clone, Copy)]
struct Variant {
    name: &'static str,
    /// Which source's tx the board holds lighting to.
    truth: Truth,
    tweak: fn(&mut Behaviour, &Keyboard),
}

#[derive(Clone, Copy, PartialEq)]
enum Truth {
    OpenRgb,
    OpenRazer,
    SignalRgb,
}

const VARIANTS: &[Variant] = &[
    Variant { name: "strict, OpenRGB's tx", truth: Truth::OpenRgb, tweak: |_, _| {} },
    Variant { name: "strict, openrazer's tx", truth: Truth::OpenRazer, tweak: |_, _| {} },
    Variant { name: "strict, SignalRGB's row tx", truth: Truth::SignalRgb, tweak: |_, _| {} },
    // Legacy boards keep their split brain here: the Chroma V2 answers getters and brightness at
    // its default tx (live), so a whole-protocol cohort contradicts the family's own evidence.
    Variant {
        name: "every request held to the cohort",
        truth: Truth::OpenRgb,
        tweak: |b, _| {
            if b.family == Family::Extended {
                b.scope = Scope::All;
            }
        },
    },
    Variant {
        name: "driver mode required",
        truth: Truth::OpenRazer,
        tweak: |b, _| b.needs_driver_mode = true,
    },
    Variant {
        name: "reads at any tx, writes held to openrazer's",
        truth: Truth::OpenRazer,
        tweak: |b, _| {
            if b.family == Family::Extended {
                b.scope = Scope::Writes;
            }
        },
    },
    // openrazer reads the V4 X at 0x1F and writes its effects at 0x3F: getters at one tx, writes
    // at another, nothing answered anywhere else.
    Variant {
        name: "reads at openrazer's getter tx, writes at its effect tx",
        truth: Truth::OpenRazer,
        tweak: |b, k| {
            if b.family == Family::Extended {
                b.scope = Scope::All;
                b.read_cohort = k.openrazer.as_ref().and_then(|o| o.getter_tx).map(|t| vec![t]);
            }
        },
    },
    Variant {
        name: "driver mode required, every request held to the cohort",
        truth: Truth::OpenRgb,
        tweak: |b, _| {
            b.needs_driver_mode = true;
            if b.family == Family::Extended {
                b.scope = Scope::All;
            }
        },
    },
    Variant {
        name: "reads at any LED, writes on one",
        truth: Truth::OpenRgb,
        tweak: |b, _| b.lenient_led_reads = true,
    },
    Variant {
        name: "lenient tx, LED 0x00 is the whole board",
        truth: Truth::OpenRgb,
        tweak: |b, _| {
            b.foreign_getter = Refusal::Honour;
            b.foreign_write = Refusal::Honour;
            b.zero_led_is_all = true;
        },
    },
];

/// The sources' value for `truth`, falling back to the other when it names none.
fn tx_for(k: &Keyboard, truth: Truth) -> Option<u8> {
    let or = k.openrazer.as_ref().map(|o| o.effect_tx);
    let rgb = k.openrgb.as_ref().map(|o| o.tx);
    let sig = k.signalrgb.as_ref().and_then(|o| o.row_tx);
    match truth {
        Truth::OpenRgb => rgb.or(or),
        Truth::OpenRazer => or.or(rgb),
        Truth::SignalRgb => sig.or(rgb).or(or),
    }
}

fn led_for(k: &Keyboard, truth: Truth) -> u8 {
    let rgb = k.openrgb.as_ref().and_then(|o| o.led);
    match truth {
        Truth::OpenRgb | Truth::SignalRgb => rgb.unwrap_or(0x05),
        // openrazer's keyboard builders address the backlight LED.
        Truth::OpenRazer => 0x05,
    }
}

/// The unit a family enumerates as: a boot-keyboard collection with no feature report, a decoy
/// that answers identity getters only, and the control collection where the atlas puts it.
fn unit_for(k: &Keyboard, pid: u16, tag: &str, b: Behaviour) -> Unit {
    let (page, usage) = k.collections().first().map_or((0x0001, 0x0002), |c| (c.usage_page, c.usage));
    let iface = k.interface().preferred().unwrap_or(2).max(1);
    let decoy_iface = if iface == 1 { 2 } else { 1 };
    Unit::new(pid, &format!("Phantom {}", k.name), tag, b)
        .collection(0, 0x0001, 0x0006, 0, Role::Mute)
        .collection(decoy_iface, 0x0001, 0x0002, 91, Role::IdentityOnly)
        .collection(iface, page, usage, 91, Role::Control)
}

fn behaviour_for(k: &Keyboard, v: Variant) -> Option<Behaviour> {
    let (rows, cols) = k.dims().preferred().filter(|&(r, c)| r > 0 && c > 0)?;
    let tx = tx_for(k, v.truth)?;
    let mut b = match k.class().preferred()? {
        Class::Extended => {
            let mut b = Behaviour::extended(&[tx], rows, cols);
            b.leds = vec![led_for(k, v.truth)];
            b
        }
        Class::Standard => Behaviour::standard(tx, rows, cols),
        _ => return None,
    };
    (v.tweak)(&mut b, k);
    Some(b)
}

/// What a case checks after adoption.
struct Expect {
    frame: bool,
}

fn run_case(k: &Keyboard, pid: u16, v: Variant, expect: &Expect) -> Result<()> {
    let behaviour = behaviour_for(k, v).context("no testable shape")?;
    let _run_root = IsolatedRunRoot::new(&format!("{pid:04x}"))?;
    let mut backend = FirmwareBackend::new();
    let board: Arc<Board> = backend.with(unit_for(k, pid, &format!("{pid:04x}"), behaviour.clone()));
    let _bus = transport::install_backend(Arc::new(backend));

    let adoption = synth::adopt_unknown(&Registry::load()?)?;
    ensure!(adoption.adopted.len() == 1, "not adopted: {:?}", adoption.skipped);
    let mut def = Registry::load()?.find_by_pid(0x1532, pid).context("adopted file did not load")?.clone();
    ensure!(def.lighting.is_some(), "no lighting block: {:?}", def.commands.keys().collect::<Vec<_>>());

    // The app's first-light settle (glue::maybe_first_light_heal), once per unit.
    let level_before = board.brightness(behaviour.leds[0]);
    let settled = neuron::calibrate::settle(&def, pid, "")?.context("nothing settled")?;
    ensure!(
        matches!(settled.verdict, neuron::calibrate::Verdict::Verified | neuron::calibrate::Verdict::Moved),
        "settle verdict {:?}: {:?}",
        settled.verdict,
        neuron::calibrate::record_for("razer", pid).map(|r| r.notes)
    );
    if !settled.changes.is_empty() {
        def = Registry::load()?.find_by_pid(0x1532, pid).context("settled file did not load")?.clone();
        ensure!(def == settled.def, "the rewritten file reloads to the settled definition");
    }
    ensure!(board.brightness(behaviour.leds[0]) == level_before, "settling left brightness changed");
    ensure!(neuron::calibrate::settle(&def, pid, "")?.is_none(), "a second settle repeated the first");
    ensure!(board.decoy_writes().is_empty(), "writes reached the decoy collection: {:?}", board.decoy_writes());

    let lighting = def.lighting.clone().context("no lighting block")?;
    let device = Device::open(def.clone(), pid)?;
    let lights = Lights::new(&device, lighting.clone());

    let red = Rgb::new(0xFF, 0x00, 0x00);
    lights.set_effect(Effect::Static, Some(red), false)?;
    match board.shown() {
        Shown::Effect { effect, color: Some(c), led } if c == red && matches!(effect, 0x01 | 0x06) => {
            ensure!(behaviour.leds.contains(&led) || (led == 0 && behaviour.zero_led_is_all), "static on LED {led:02X}");
        }
        other => bail!("static red shows {other:?}"),
    }

    if lighting.brightness.is_some() {
        capability::set_brightness(&device, 50, Store::Volatile)?;
        let led = if lighting.protocol == neuron::lighting::Protocol::Legacy { 0x05 } else { lighting.led_id };
        let level = board.brightness(if led == 0 { behaviour.leds[0] } else { led });
        ensure!(level == Some(127), "brightness reads {level:?}");
    } else {
        bail!("no brightness write: {:?}", def.commands.keys().collect::<Vec<_>>());
    }

    if expect.frame {
        let (rows, cols) = (lighting.rows as usize, lighting.cols as usize);
        let frame: Vec<Rgb> = (0..rows * cols).map(|i| Rgb::new(i as u8, (i * 3) as u8, 0x40)).collect();
        lights.paint_frame(&frame)?;
        ensure!(board.shown() == Shown::Frame, "the custom frame is not displayed: {:?}", board.shown());
        ensure!(board.frame() == frame, "the displayed frame differs from the painted one");
    }
    // Volatile throughout: no applied matrix write carries VARSTORE, and no legacy write but the
    // brightness the Chroma V2's own block persists.
    let varstore: Vec<_> = board
        .applied()
        .into_iter()
        .filter(|w| w.class == 0x0F && w.args.first() == Some(&0x01))
        .collect();
    ensure!(varstore.is_empty(), "VARSTORE writes: {varstore:?}");
    Ok(())
}

/// One family per distinct wire shape, so the default run stays short; `--ignored` runs all.
fn families(all: bool) -> Vec<&'static Keyboard> {
    let mut seen = Vec::new();
    atlas::keyboards()
        .iter()
        .filter(|k| (all || !k.is_laptop()) && k.pid != 0x0221)
        .filter(|k| behaviour_for(k, VARIANTS[0]).is_some())
        .filter(|k| {
            if ANCHORS.contains(&k.pid) {
                return true;
            }
            let sig = (
                k.class().preferred(),
                k.lighting_tx(),
                k.led(),
                k.collections(),
                k.interface().preferred(),
                k.dims().preferred(),
            );
            let fresh = !seen.contains(&sig);
            seen.push(sig);
            all || fresh
        })
        .collect()
}

/// Boards every assumption runs on by default: the BlackWidow V3 and V4 X (the reported boards)
/// and the BlackWidow Chroma (a learned legacy board).
const ANCHORS: [u16; 3] = [0x024E, 0x0293, 0x0203];

fn run_matrix(all: bool) -> Result<()> {
    let _serial = transport::mock::test_lock().lock().unwrap_or_else(PoisonError::into_inner);
    let _deny = transport::deny_hardware();
    let mut failures = Vec::new();
    let mut cases = 0;
    for k in families(all) {
        // By default every shape runs under both strict readings of its sources, and the anchors
        // under every assumption; `--ignored` runs the full product.
        let variants: &[Variant] = if all || ANCHORS.contains(&k.pid) { VARIANTS } else { &VARIANTS[..2] };
        for v in variants {
            cases += 1;
            if let Err(e) = run_case(k, k.pid, *v, &Expect { frame: true }) {
                failures.push(format!("{:04x} {} [{}]: {e:#}", k.pid, k.name, v.name));
            }
        }
    }
    ensure!(cases > 40, "the matrix shrank to {cases} cases");
    if !failures.is_empty() {
        bail!("{} of {cases} cases failed:\n{}", failures.len(), failures.join("\n"));
    }
    Ok(())
}

#[test]
fn every_keyboard_shape_lights_under_every_assumption() -> Result<()> {
    run_matrix(false)
}

#[test]
#[ignore = "every atlas keyboard; run with --ignored"]
fn every_atlas_keyboard_lights_under_every_assumption() -> Result<()> {
    run_matrix(true)
}

/// Discovery without priors: the V3 and V4 X shapes under a PID the atlas does not know. The
/// matrix size falls back to 6x22, which both boards are, so frames are checked too.
#[test]
fn unknown_pids_are_found_by_probing_alone() -> Result<()> {
    let _serial = transport::mock::test_lock().lock().unwrap_or_else(PoisonError::into_inner);
    let _deny = transport::deny_hardware();
    for (known, blind) in [(0x024E, 0x7E01), (0x0293, 0x7E02)] {
        ensure!(atlas::lookup(0x1532, blind).is_none(), "{blind:04x} must be unknown to the atlas");
        let k = atlas::lookup(0x1532, known).context("atlas entry")?;
        for v in &VARIANTS[..2] {
            run_case(k, blind, *v, &Expect { frame: true })
                .with_context(|| format!("{known:04x} as {blind:04x} [{}]", v.name))?;
        }
    }
    Ok(())
}

/// A board that never answers a foreign tx (silence, not a status byte) costs the probe a
/// timeout per cohort but is still found.
#[test]
fn a_silent_foreign_tx_is_survived() -> Result<()> {
    let _serial = transport::mock::test_lock().lock().unwrap_or_else(PoisonError::into_inner);
    let _deny = transport::deny_hardware();
    let k = atlas::lookup(0x1532, 0x024E).context("V3")?;
    let silent = Variant {
        name: "foreign tx silent",
        truth: Truth::OpenRazer,
        tweak: |b, _| {
            b.foreign_getter = Refusal::Silent;
            b.foreign_write = Refusal::Silent;
        },
    };
    run_case(k, 0x7E03, silent, &Expect { frame: true })
}
