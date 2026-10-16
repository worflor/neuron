// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo Research Components Exception 1.0.
// See ../../../LICENSE.md.

use anyhow::{ensure, Result};
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
        0x11, 0xFF, 0x00, 0x1A, 0x00, 0x00, 0x5A, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    ];
    const BATTERY_INDEX: [u8; 20] = [
        0x11, 0xFF, 0x00, 0x0A, 0x10, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    ];
    const DPI_INDEX: [u8; 20] = [
        0x11, 0xFF, 0x00, 0x0A, 0x22, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    ];

    let failed = tape(vec![exchange(
        PING,
        vec![
            Reply::Bytes(vec![0x11, 0xFF, 0x00, 0x1A]),
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
    failed_verifier.verify_complete()?;
    drop(failed_backend);

    let adopted_transcript = tape(vec![
        exchange(
            PING,
            vec![
                Reply::Bytes(vec![0x11, 0xFF, 0x00, 0x1A]),
                Reply::Bytes(vec![0x10, 0xFF, 0x00, 0x1A, 0x02, 0x00, 0x5A]),
            ],
        ),
        exchange(BATTERY_INDEX, vec![Reply::Bytes(vec![0x10, 0xFF, 0x00, 0x0A, 0x06, 0x00, 0x00])]),
        exchange(DPI_INDEX, vec![Reply::Bytes(vec![0x10, 0xFF, 0x00, 0x0A, 0x07, 0x00, 0x00])]),
        exchange(
            [
                0x11, 0xFF, 0x06, 0x0A, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
                0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            ],
            vec![Reply::Bytes(vec![0x10, 0xFF, 0x06, 0x0A, 0x55, 0x00, 0x00])],
        ),
        exchange(
            [
                0x11, 0xFF, 0x07, 0x2A, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
                0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            ],
            vec![Reply::Bytes(vec![0x10, 0xFF, 0x07, 0x2A, 0x00, 0x03, 0x20])],
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
    verifier.verify_complete()?;
    Ok(())
}
