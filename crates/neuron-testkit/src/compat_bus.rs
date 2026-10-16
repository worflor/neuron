// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo Research Components Exception 1.0.
// See ../../../LICENSE.md.

//! Offline HID enumeration and protocol replay for compatibility tests.
//!
//! This backend only serves reviewed [`crate::compat::ProtocolTape`] conversations. It never
//! falls through to a platform backend, and it deliberately has no invented input-report path.

use crate::compat::RecordedProtocol;
use anyhow::{bail, ensure, Result};
use neuron::transport::{Backend, DevicePath, HidDeviceInfo, InputReader, Transport};
use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};

struct ConnectionState {
    connected: bool,
    generation: u64,
}

struct ReplayDevice {
    info: HidDeviceInfo,
    protocol: RecordedProtocol,
    connection: Mutex<ConnectionState>,
}

/// A deterministic virtual HID bus backed only by protocol recordings.
///
/// Enumeration preserves the input order. Disconnecting removes the path and permanently stales
/// its open handles; reconnecting makes it available to newly opened handles.
pub struct ReplayBackend {
    devices: Vec<Arc<ReplayDevice>>,
    by_path: HashMap<DevicePath, Arc<ReplayDevice>>,
}

impl ReplayBackend {
    /// Build a virtual bus. Each tape's recorded VID/PID must match its enumerated interface, and
    /// every interface path must be unique.
    pub fn new(devices: Vec<(HidDeviceInfo, RecordedProtocol)>) -> Result<Self> {
        let mut entries = Vec::with_capacity(devices.len());
        let mut by_path = HashMap::with_capacity(devices.len());

        for (info, protocol) in devices {
            ensure!(!by_path.contains_key(&info.path), "duplicate replay interface path");
            let (vid, pid) = protocol.identity();
            ensure!(
                (vid, pid) == (info.vid, info.pid),
                "protocol tape identity {vid:04x}:{pid:04x} does not match interface {:04x}:{:04x}",
                info.vid,
                info.pid
            );
            let entry = Arc::new(ReplayDevice {
                info,
                protocol,
                connection: Mutex::new(ConnectionState { connected: true, generation: 0 }),
            });
            by_path.insert(entry.info.path.clone(), Arc::clone(&entry));
            entries.push(entry);
        }

        Ok(ReplayBackend { devices: entries, by_path })
    }

    /// Connect or disconnect one interface. Reconnecting makes the path available to new handles;
    /// handles opened before a disconnect remain stale.
    pub fn set_connected(&self, path: &DevicePath, connected: bool) -> Result<()> {
        let device = self.by_path.get(path).ok_or_else(|| anyhow::anyhow!("unknown replay interface path"))?;
        let mut state = device.connection.lock().unwrap_or_else(PoisonError::into_inner);
        if state.connected == connected {
            return Ok(());
        }
        if connected {
            let Some(generation) = state.generation.checked_add(1) else {
                state.connected = false;
                bail!("replay interface generation exhausted; interface remains disconnected");
            };
            state.generation = generation;
            state.connected = true;
        } else {
            state.connected = false;
        }
        Ok(())
    }
}

impl Backend for ReplayBackend {
    fn enumerate(&self) -> Result<Vec<HidDeviceInfo>> {
        let mut found = Vec::new();
        for device in &self.devices {
            if device.connection.lock().unwrap_or_else(PoisonError::into_inner).connected {
                let i = &device.info;
                found.push(HidDeviceInfo {
                    vid: i.vid,
                    pid: i.pid,
                    usage_page: i.usage_page,
                    usage: i.usage,
                    feature_len: i.feature_len,
                    input_len: i.input_len,
                    output_len: i.output_len,
                    path: i.path.clone(),
                    product: i.product.clone(),
                });
            }
        }
        Ok(found)
    }

    fn open_path(&self, path: &DevicePath) -> Result<Box<dyn Transport>> {
        let device = self.by_path.get(path).ok_or_else(|| anyhow::anyhow!("unknown replay interface path"))?;
        let generation = {
            let state = device.connection.lock().unwrap_or_else(PoisonError::into_inner);
            ensure!(state.connected, "replay interface is disconnected");
            state.generation
        };
        Ok(Box::new(ReplayHandle {
            device: Arc::clone(device),
            generation,
            protocol: device.protocol.clone(),
        }))
    }

    fn open_reader(&self, path: &DevicePath) -> Result<Box<dyn InputReader>> {
        let device = self.by_path.get(path).ok_or_else(|| anyhow::anyhow!("unknown replay interface path"))?;
        ensure!(
            device.connection.lock().unwrap_or_else(PoisonError::into_inner).connected,
            "replay interface is disconnected"
        );
        bail!("protocol replay has no recorded input reports")
    }
}

struct ReplayHandle {
    device: Arc<ReplayDevice>,
    generation: u64,
    protocol: RecordedProtocol,
}

impl ReplayHandle {
    fn while_connected<T>(&self, f: impl FnOnce(&RecordedProtocol) -> Result<T>) -> Result<T> {
        let state = self.device.connection.lock().unwrap_or_else(PoisonError::into_inner);
        ensure!(
            state.connected && state.generation == self.generation,
            "replay interface handle is disconnected or stale"
        );
        f(&self.protocol)
    }
}

impl Transport for ReplayHandle {
    fn set_feature(&self, buf: &[u8]) -> Result<()> {
        self.while_connected(|protocol| protocol.set_feature(buf))
    }

    fn get_feature(&self, buf: &mut [u8]) -> Result<usize> {
        self.while_connected(|protocol| protocol.get_feature(buf))
    }

    fn write_output(&self, buf: &[u8]) -> Result<()> {
        self.while_connected(|protocol| protocol.write_output(buf))
    }

    fn read_input(&self, buf: &mut [u8], timeout_ms: u32) -> Result<usize> {
        self.while_connected(|protocol| protocol.read_input(buf, timeout_ms))
    }

    fn wire_lock(&self) -> Option<Arc<neuron::transport::WireLock>> {
        self.protocol.wire_lock()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compat::{Channel, Exchange, ProtocolTape, Reply};

    fn info(pid: u16, path: &str) -> HidDeviceInfo {
        HidDeviceInfo {
            vid: 0x1532,
            pid,
            usage_page: 0xFF00,
            usage: 2,
            feature_len: 91,
            input_len: 0,
            output_len: 0,
            path: DevicePath::from_str_for_tests(path),
            product: format!("recorded {pid:04x}"),
        }
    }

    fn protocol(pid: u16, request: &[u8], reply: &[u8]) -> RecordedProtocol {
        RecordedProtocol::new(ProtocolTape {
            schema: 1,
            vid: 0x1532,
            pid,
            exchanges: vec![Exchange {
                channel: Channel::Feature,
                request: request.to_vec(),
                replies: vec![Reply::Bytes(reply.to_vec())],
            }],
        })
        .unwrap()
    }

    #[test]
    fn constructor_rejects_duplicate_paths_and_tape_identity_mismatch() {
        let request = [1, 2];
        let reply = [3, 4];
        assert!(ReplayBackend::new(vec![
            (info(0x4567, "same-path"), protocol(0x4567, &request, &reply)),
            (info(0x4568, "same-path"), protocol(0x4568, &request, &reply)),
        ])
        .is_err());

        assert!(ReplayBackend::new(vec![
            (info(0x4567, "identity-mismatch"), protocol(0x4568, &request, &reply)),
        ])
        .is_err());
    }

    #[test]
    fn enumeration_disconnect_retained_handle_and_reconnect_are_offline() {
        let path_a = DevicePath::from_str_for_tests("replay-a");
        let request = [0xA1, 0xB2];
        let first_response = [0xC3, 0xD4];
        let second_response = [0xE5, 0xF6];
        let transcript = RecordedProtocol::new(ProtocolTape {
            schema: 1,
            vid: 0x1532,
            pid: 0x4567,
            exchanges: vec![first_response, second_response].into_iter().map(|response| Exchange {
                channel: Channel::Feature,
                request: request.to_vec(),
                replies: vec![Reply::Bytes(response.to_vec())],
            }).collect(),
        }).unwrap();
        let verifier = transcript.clone();
        let backend = ReplayBackend::new(vec![
            (info(0x4567, "replay-a"), transcript),
            (info(0x4568, "replay-b"), protocol(0x4568, &[0x01], &[0x02])),
        ])
        .unwrap();

        let found = backend.enumerate().unwrap();
        assert_eq!(found.iter().map(|i| i.pid).collect::<Vec<_>>(), [0x4567, 0x4568]);
        let retained = backend.open_path(&path_a).unwrap();
        let generation = backend.by_path[&path_a].connection.lock().unwrap().generation;
        backend.set_connected(&path_a, true).unwrap();
        assert_eq!(backend.by_path[&path_a].connection.lock().unwrap().generation, generation,
            "repeating the connected state is idempotent");
        retained.set_feature(&request).unwrap();
        let mut first = [0; 8];
        assert_eq!(retained.get_feature(&mut first).unwrap(), first_response.len());
        assert_eq!(&first[..first_response.len()], &first_response);

        backend.set_connected(&path_a, false).unwrap();
        let disconnected_generation = backend.by_path[&path_a].connection.lock().unwrap().generation;
        backend.set_connected(&path_a, false).unwrap();
        assert_eq!(backend.by_path[&path_a].connection.lock().unwrap().generation, disconnected_generation,
            "repeating the disconnected state is idempotent");
        assert_eq!(backend.enumerate().unwrap().iter().map(|i| i.pid).collect::<Vec<_>>(), [0x4568]);
        assert!(backend.open_path(&path_a).is_err(), "offline paths cannot be reopened");
        assert!(retained.set_feature(&request).is_err(), "retained handles fail while offline");

        backend.set_connected(&path_a, true).unwrap();
        let reconnected_generation = backend.by_path[&path_a].connection.lock().unwrap().generation;
        assert_eq!(reconnected_generation, disconnected_generation + 1);
        backend.set_connected(&path_a, true).unwrap();
        assert_eq!(backend.by_path[&path_a].connection.lock().unwrap().generation, reconnected_generation,
            "repeating reconnect is idempotent");
        assert_eq!(backend.enumerate().unwrap().iter().map(|i| i.pid).collect::<Vec<_>>(), [0x4567, 0x4568]);
        assert!(retained.set_feature(&request).is_err(), "reconnect does not revive the old handle");
        let reopened = backend.open_path(&path_a).unwrap();
        reopened.set_feature(&request).unwrap();
        let mut got = [0; 8];
        assert_eq!(reopened.get_feature(&mut got).unwrap(), second_response.len());
        assert_eq!(&got[..second_response.len()], &second_response);
        verifier.verify_complete().unwrap();
    }

    #[test]
    fn generation_overflow_leaves_the_interface_offline() {
        let path = DevicePath::from_str_for_tests("generation-overflow");
        let backend = ReplayBackend::new(vec![
            (info(0x4567, "generation-overflow"), protocol(0x4567, &[1], &[2])),
        ])
        .unwrap();
        {
            let mut state = backend.by_path[&path].connection.lock().unwrap();
            state.connected = false;
            state.generation = u64::MAX;
        }

        assert!(backend.set_connected(&path, true).is_err());
        assert!(backend.enumerate().unwrap().is_empty());
        assert!(backend.open_path(&path).is_err());
    }

    #[test]
    fn unknown_paths_and_unrecorded_input_fail_closed() {
        let path = DevicePath::from_str_for_tests("known");
        let backend = ReplayBackend::new(vec![
            (info(0x4567, "known"), protocol(0x4567, &[1], &[2])),
        ])
        .unwrap();
        let unknown = DevicePath::from_str_for_tests("unknown");
        assert!(backend.open_path(&unknown).is_err());
        assert!(backend.set_connected(&unknown, false).is_err());
        assert!(backend
            .open_reader(&path)
            .err()
            .is_some_and(|e| e.to_string().contains("no recorded input")));
    }
}
