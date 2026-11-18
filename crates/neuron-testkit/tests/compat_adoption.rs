// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo Research Components Exception 1.0.
// See ../../../LICENSE.md.

use anyhow::{ensure, Context, Result};
use neuron::{
    capability,
    device::Device,
    registry::Registry,
    runroot::RunDirPin,
    synth,
    transport::{self, DevicePath, HidDeviceInfo},
};
use neuron_testkit::{
    compat::{Channel, Exchange, ProtocolTape, RecordedProtocol, Reply},
    compat_bus::ReplayBackend,
};
use std::{
    path::PathBuf,
    sync::{Arc, PoisonError},
    time::{SystemTime, UNIX_EPOCH},
};

// Report and feature layouts below are source-derived from Logitech/cpg-docs at
// f9107a4e3190d798f9be8c5d596f255f5e5741d2 (HID++ 2.0 reports, IRoot, BatteryLevelStatus,
// AdjustableDPI) and pwr-Solaar/Solaar at e7304c4c451cc9bb4f206a914844525e67856a28
// (lib/logitech_receiver/base.py request/reply framing). Replies are synthetic, not captures.

struct IsolatedRunRoot {
    path: PathBuf,
    _pin: RunDirPin,
}

impl IsolatedRunRoot {
    fn new() -> Result<Self> {
        let nonce = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_nanos();
        let path = std::env::temp_dir().join(format!(
            "neuron_compat_adoption_{}_{}",
            std::process::id(),
            nonce
        ));
        std::fs::create_dir(&path)?;
        let pin = RunDirPin::to(&path);
        Ok(Self { path, _pin: pin })
    }
}

impl Drop for IsolatedRunRoot {
    fn drop(&mut self) {
        // RunDirPin remains alive until after the private directory is removed.
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

fn direct_interface() -> HidDeviceInfo {
    HidDeviceInfo {
        vid: 0x046D,
        pid: 0xF201,
        usage_page: 0xFF00,
        usage: 2,
        feature_len: 0,
        input_len: 20,
        output_len: 20,
        path: DevicePath::from_str_for_tests("compat-adoption-direct"),
        product: "Source-derived HID++ direct device".into(),
    }
}

fn tape(exchanges: Vec<Exchange>) -> Result<RecordedProtocol> {
    RecordedProtocol::new(ProtocolTape { schema: 1, vid: 0x046D, pid: 0xF201, exchanges })
}

/// IRoot.getFeature for `feature`, as neuron frames it.
fn feature_index(feature: u16) -> [u8; 20] {
    let mut r = [0u8; 20];
    r[..4].copy_from_slice(&[0x11, 0xFF, 0x00, 0x0C]);
    r[4..6].copy_from_slice(&feature.to_be_bytes());
    r
}

fn exchange(request: [u8; 20], replies: Vec<Reply>) -> Exchange {
    Exchange { channel: Channel::OutputInput, request: request.to_vec(), replies }
}

#[test]
fn adoption_and_reloaded_hidpp_getters_use_the_recorded_bus() -> Result<()> {
    let _serial = transport::mock::test_lock().lock().unwrap_or_else(PoisonError::into_inner);
    let _deny = transport::deny_hardware();
    let _run_root = IsolatedRunRoot::new()?;
    let pid = 0xF201;
    let auto_file = synth::auto_def_path("hidpp", pid);

    const PING: [u8; 20] = [
        0x11, 0xFF, 0x00, 0x1C, 0x00, 0x00, 0x5A, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    ];
    const BATTERY_INDEX: [u8; 20] = [
        0x11, 0xFF, 0x00, 0x0C, 0x10, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    ];
    const DPI_INDEX: [u8; 20] = [
        0x11, 0xFF, 0x00, 0x0C, 0x22, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    ];
    const NAME_INDEX: [u8; 20] = [
        0x11, 0xFF, 0x00, 0x0C, 0x00, 0x05, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    ];

    let failed = tape(vec![exchange(
        PING,
        vec![
            Reply::Bytes(vec![0x11, 0xFF, 0x00, 0x1C]),
            Reply::Timeout,
            Reply::Timeout,
            Reply::Timeout,
            Reply::Timeout,
            Reply::Timeout,
            Reply::Timeout,
            Reply::Timeout,
            Reply::Timeout,
            Reply::Timeout,
            Reply::Timeout,
        ],
    )])?;
    let failed_verifier = failed.clone();
    let failed_bus = Arc::new(ReplayBackend::new(vec![(direct_interface(), failed)])?);
    let failed_backend = transport::install_backend(failed_bus);
    let initial_registry = Registry::load()?;
    let initial = synth::adopt_unknown(&initial_registry)?;
    ensure!(initial.adopted.is_empty(), "header-only ping must not be adopted");
    ensure!(!auto_file.exists(), "failed ping must not write an auto definition");
    failed_verifier.verify_complete().context("failed-ping phase")?;
    drop(failed_backend);

    let adopted_transcript = tape(vec![
        exchange(
            PING,
            vec![
                Reply::Bytes(vec![0x11, 0xFF, 0x00, 0x1C]),
                Reply::Bytes(vec![0x10, 0xFF, 0x00, 0x1C, 0x02, 0x00, 0x5A]),
            ],
        ),
        exchange(BATTERY_INDEX, vec![Reply::Bytes(vec![0x10, 0xFF, 0x00, 0x0C, 0x06, 0x00, 0x00])]),
        exchange(DPI_INDEX, vec![Reply::Bytes(vec![0x10, 0xFF, 0x00, 0x0C, 0x07, 0x00, 0x00])]),
        exchange(NAME_INDEX, vec![Reply::Bytes(vec![0x10, 0xFF, 0x00, 0x0C, 0x00, 0x00, 0x00])]),
        // the lighting features 0x8081, 0x8071 and 0x8070: absent
        exchange(feature_index(0x8081), vec![Reply::Bytes(vec![0x10, 0xFF, 0x00, 0x0C, 0x00, 0x00, 0x00])]),
        exchange(feature_index(0x8071), vec![Reply::Bytes(vec![0x10, 0xFF, 0x00, 0x0C, 0x00, 0x00, 0x00])]),
        exchange(feature_index(0x8070), vec![Reply::Bytes(vec![0x10, 0xFF, 0x00, 0x0C, 0x00, 0x00, 0x00])]),
        exchange(
            [
                0x11, 0xFF, 0x06, 0x0C, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
                0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            ],
            vec![Reply::Bytes(vec![0x10, 0xFF, 0x06, 0x0C, 0x55, 0x00, 0x00])],
        ),
        exchange(
            [
                0x11, 0xFF, 0x07, 0x2C, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
                0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            ],
            vec![Reply::Bytes(vec![0x10, 0xFF, 0x07, 0x2C, 0x00, 0x03, 0x20])],
        ),
    ])?;
    let verifier = adopted_transcript.clone();
    let bus = Arc::new(ReplayBackend::new(vec![(direct_interface(), adopted_transcript)])?);
    let _backend = transport::install_backend(bus);
    let discovery_registry = Registry::load()?;
    let adopted = synth::adopt_unknown(&discovery_registry)?;
    ensure!(
        adopted.adopted.len() == 1,
        "expected one newly adopted direct device: {:?}",
        adopted.skipped
    );
    ensure!(adopted.adopted[0].pid == pid && adopted.adopted[0].dialect == "hidpp");
    ensure!(auto_file.is_file(), "successful adoption must emit its auto definition");

    let registry = Registry::load()?;
    let def = registry
        .find_by_pid(0x046D, pid)
        .ok_or_else(|| anyhow::anyhow!("reloaded HID++ definition missing"))?;
    let mut commands = def.commands.keys().map(String::as_str).collect::<Vec<_>>();
    commands.sort_unstable();
    ensure!(commands == vec!["battery_level", "dpi"], "adopted commands were {commands:?}");
    ensure!(!def.commands.contains_key("set_dpi"), "adoption must not synthesize setters");
    let device = Device::open(def.clone(), pid)?;
    ensure!(capability::battery_percent(&device)? == 85, "battery getter did not decode 85 percent");
    ensure!(capability::dpi(&device)? == (800, 800), "DPI getter did not decode 800 DPI");
    verifier.verify_complete().context("adopted phase")?;
    Ok(())
}

// VIA layouts are source-derived from qmk/qmk_firmware at 8252b6309752f0be6882c387e15989ba51323d8f
// (quantum/via.c and via.h, tmk_core/protocol/usb_descriptor_common.h). Replies are synthetic.
fn via_interface() -> HidDeviceInfo {
    HidDeviceInfo {
        vid: 0x3434,
        pid: 0x0281,
        usage_page: 0xFF60,
        usage: 0x61,
        feature_len: 0,
        input_len: 33,
        output_len: 33,
        path: DevicePath::from_str_for_tests(r"\\?\hid#vid_3434&pid_0281&mi_01#7&via&0&0000#{4d1e55b2-f16f-11cf-88cb-001111000030}"),
        product: "Source-derived QMK VIA keyboard".into(),
    }
}

/// The same unit's boot keyboard: what tells adoption the raw HID collection is on a keyboard.
fn via_keyboard_interface() -> HidDeviceInfo {
    HidDeviceInfo {
        usage_page: 0x0001,
        usage: 0x0006,
        input_len: 9,
        output_len: 2,
        path: DevicePath::from_str_for_tests(r"\\?\hid#vid_3434&pid_0281&mi_00#7&via&0&0000#{4d1e55b2-f16f-11cf-88cb-001111000030}"),
        ..via_interface()
    }
}

fn via_report(data: &[u8]) -> Vec<u8> {
    let mut v = vec![0u8; 33];
    v[1..1 + data.len()].copy_from_slice(data);
    v
}

#[test]
fn a_via_keyboard_is_adopted_and_its_brightness_read_on_the_recorded_bus() -> Result<()> {
    let _serial = transport::mock::test_lock().lock().unwrap_or_else(PoisonError::into_inner);
    let _deny = transport::deny_hardware();
    let _run_root = IsolatedRunRoot::new()?;
    let pid = 0x0281;
    let auto_file = synth::auto_def_path_for("via", 0x3434, pid);

    let ask = |data: &[u8], reply: &[u8]| Exchange {
        channel: Channel::OutputInput,
        request: via_report(data),
        replies: vec![Reply::Bytes(via_report(reply))],
    };
    let tape = RecordedProtocol::new(ProtocolTape {
        schema: 1,
        vid: 0x3434,
        pid,
        exchanges: vec![
            ask(&[0x01], &[0x01, 0x00, 0x0C]),
            // each lighting read goes out with two fillers; an answer writes over them
            ask(&[0x08, 0x03, 0x01, 0xA5], &[0x08, 0x03, 0x01, 200]),
            ask(&[0x08, 0x03, 0x01, 0x5A], &[0x08, 0x03, 0x01, 200]),
            ask(&[0x08, 0x03, 0x04, 0xA5, 0xA5], &[0x08, 0x03, 0x04, 0x30, 0xFF]),
            ask(&[0x08, 0x03, 0x04, 0x5A, 0x5A], &[0x08, 0x03, 0x04, 0x30, 0xFF]),
            ask(&[0x08, 0x03, 0x01], &[0x08, 0x03, 0x01, 200]),
        ],
    })?;
    let verifier = tape.clone();
    let keyboard = RecordedProtocol::new(ProtocolTape { schema: 1, vid: 0x3434, pid, exchanges: Vec::new() })?;
    let _backend = transport::install_backend(Arc::new(ReplayBackend::new(vec![
        (via_interface(), tape),
        (via_keyboard_interface(), keyboard),
    ])?));

    let adopted = synth::adopt_unknown(&Registry::load()?)?;
    ensure!(adopted.adopted.len() == 1, "expected the VIA board adopted: {:?}", adopted.skipped);
    ensure!(adopted.adopted[0].dialect == "via");
    ensure!(auto_file.is_file(), "adoption writes the learned file");

    let registry = Registry::load()?;
    let def = registry.find_by_pid(0x3434, pid).ok_or_else(|| anyhow::anyhow!("reloaded VIA def missing"))?;
    ensure!(
        def.commands.keys().map(String::as_str).collect::<Vec<_>>() == ["brightness", "set_brightness"],
        "{:?}",
        def.commands.keys()
    );
    let lighting = def.lighting.as_ref().ok_or_else(|| anyhow::anyhow!("the board's colour was learned"))?;
    ensure!(
        lighting.protocol == neuron::lighting::Protocol::Via && lighting.channel == Some(0x03),
        "one colour on the RGB matrix channel"
    );
    let device = Device::open(def.clone(), pid)?;
    ensure!(capability::brightness_percent(&device)? == 78, "200/255 reads as 78%");
    verifier.verify_complete().context("VIA phase")?;
    Ok(())
}
