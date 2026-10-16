// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo Research Components Exception 1.0.
// See ../../../LICENSE.md.

//! Offline compatibility fixtures for production input decoders and protocol conversations.
//! These adapters never enumerate or open hardware and cannot enable dispatch.

use anyhow::{ensure, Result};
use neuron::compat::InputRecording;
use neuron::transport::{InputReader, Transport};
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

pub struct RecordedInput(Mutex<VecDeque<Vec<u8>>>);

impl RecordedInput {
    /// Validate the collector recipe and retain reports in their recorded order.
    /// Timestamps remain in the recording; reads are driven by the test, without sleeping.
    pub fn new(recording: &InputRecording) -> Result<Self> {
        neuron::compat::analyze(recording)?;
        Ok(Self(Mutex::new(recording.samples.iter().map(|s| s.bytes.clone()).collect())))
    }
}

impl InputReader for RecordedInput {
    fn read(&self, buffer: &mut [u8]) -> Result<Option<usize>> {
        let mut reports = self.0.lock().map_err(|_| anyhow::anyhow!("recorded input state poisoned"))?;
        let Some(next) = reports.front() else { return Ok(None); };
        ensure!(next.len() <= buffer.len(), "replay buffer is smaller than the recorded report");
        let n = next.len();
        buffer[..n].copy_from_slice(next);
        reports.pop_front();
        Ok(Some(n))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Channel { Feature, OutputInput }

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", content = "bytes", rename_all = "snake_case", deny_unknown_fields)]
pub enum Reply { Bytes(Vec<u8>), Timeout, Disconnected }

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Exchange {
    pub channel: Channel,
    pub request: Vec<u8>,
    pub replies: Vec<Reply>,
}

/// A reviewed request/reply fixture. This format does not authorize live command collection.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProtocolTape {
    pub schema: u32,
    pub vid: u16,
    pub pid: u16,
    pub exchanges: Vec<Exchange>,
}

struct State {
    exchanges: VecDeque<Exchange>,
    pending: Option<(Channel, VecDeque<Reply>)>,
    violation: Option<String>,
}

impl State {
    fn reject(&mut self, message: &str) -> anyhow::Error {
        self.violation.get_or_insert_with(|| message.to_string());
        anyhow::anyhow!("{message}")
    }
}

/// Clones share one conversation and its verification history, including across reopened handles.
#[derive(Clone)]
pub struct RecordedProtocol {
    state: Arc<Mutex<State>>,
    identity: (u16, u16),
}

impl RecordedProtocol {
    pub fn new(tape: ProtocolTape) -> Result<Self> {
        ensure!(tape.schema == 1 && tape.exchanges.len() <= 1024, "invalid protocol tape schema or size");
        for exchange in &tape.exchanges {
            ensure!(!exchange.request.is_empty() && exchange.request.len() <= 4096
                && !exchange.replies.is_empty() && exchange.replies.len() <= 1024, "invalid exchange bounds");
            for reply in &exchange.replies {
                if let Reply::Bytes(bytes) = reply { ensure!(bytes.len() <= 4096, "reply exceeds 4096 bytes"); }
            }
        }
        Ok(Self { identity: (tape.vid, tape.pid), state: Arc::new(Mutex::new(State {
            exchanges: tape.exchanges.into(), pending: None, violation: None,
        })) })
    }

    #[must_use]
    pub fn identity(&self) -> (u16, u16) { self.identity }

    fn send(&self, channel: Channel, bytes: &[u8]) -> Result<()> {
        let mut state = self.state.lock().map_err(|_| anyhow::anyhow!("protocol replay state poisoned"))?;
        if state.pending.is_some() { return Err(state.reject("previous recorded conversation was not drained")); }
        let Some(next) = state.exchanges.front() else { return Err(state.reject("no recorded request remains")); };
        if next.channel != channel || next.request != bytes {
            return Err(state.reject("request does not match recorded bytes or channel"));
        }
        let next = state.exchanges.pop_front().ok_or_else(|| anyhow::anyhow!("recorded request disappeared"))?;
        state.pending = Some((channel, next.replies.into()));
        Ok(())
    }

    fn receive(&self, channel: Channel, buffer: &mut [u8]) -> Result<usize> {
        let mut state = self.state.lock().map_err(|_| anyhow::anyhow!("protocol replay state poisoned"))?;
        let Some((expected, replies)) = state.pending.as_ref() else { return Err(state.reject("no recorded reply is pending")); };
        if *expected != channel { return Err(state.reject("wrong reply channel")); }
        if let Some(Reply::Bytes(bytes)) = replies.front() {
            if bytes.len() > buffer.len() { return Err(state.reject("reply buffer too small")); }
        }
        let (_, replies) = state.pending.as_mut().ok_or_else(|| anyhow::anyhow!("recorded replies disappeared"))?;
        let reply = replies.pop_front().ok_or_else(|| anyhow::anyhow!("recorded replies exhausted"))?;
        if replies.is_empty() { state.pending = None; }
        match reply {
            Reply::Bytes(bytes) => { buffer[..bytes.len()].copy_from_slice(&bytes); Ok(bytes.len()) }
            Reply::Timeout => anyhow::bail!("recorded timeout"),
            Reply::Disconnected => anyhow::bail!("recorded disconnect"),
        }
    }

    /// Queue state only. Use `verify_complete` to also reject swallowed contract errors.
    pub fn drained(&self) -> Result<bool> {
        let state = self.state.lock().map_err(|_| anyhow::anyhow!("protocol replay state poisoned"))?;
        Ok(state.exchanges.is_empty() && state.pending.is_none())
    }

    /// Require every exchange to be consumed without a contract error, even if a caller swallowed it.
    pub fn verify_complete(&self) -> Result<()> {
        let state = self.state.lock().map_err(|_| anyhow::anyhow!("protocol replay state poisoned"))?;
        ensure!(state.violation.is_none(), "replay contract violated: {}", state.violation.as_deref().unwrap_or_default());
        ensure!(state.exchanges.is_empty() && state.pending.is_none(), "recorded conversation was not fully consumed");
        Ok(())
    }
}

impl Transport for RecordedProtocol {
    fn set_feature(&self, bytes: &[u8]) -> Result<()> { self.send(Channel::Feature, bytes) }
    fn get_feature(&self, buffer: &mut [u8]) -> Result<usize> { self.receive(Channel::Feature, buffer) }
    fn write_output(&self, bytes: &[u8]) -> Result<()> { self.send(Channel::OutputInput, bytes) }
    fn read_input(&self, buffer: &mut [u8], _timeout_ms: u32) -> Result<usize> { self.receive(Channel::OutputInput, buffer) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use neuron::compat::{InputSample, RecordingEnd};

    fn single_exchange(replies: Vec<Reply>) -> RecordedProtocol {
        RecordedProtocol::new(ProtocolTape { schema: 1, vid: 0x1234, pid: 0x5678,
            exchanges: vec![Exchange { channel: Channel::Feature, request: vec![1, 2], replies }] }).unwrap()
    }

    #[test]
    fn swallowed_contract_errors_remain_visible_after_the_tape_is_drained() {
        let transport = single_exchange(vec![Reply::Bytes(vec![3])]);
        let handle = transport.clone();
        assert!(handle.set_feature(&[9]).is_err());
        handle.set_feature(&[1, 2]).unwrap();
        handle.get_feature(&mut [0; 1]).unwrap();
        assert!(transport.drained().unwrap());
        assert!(transport.verify_complete().unwrap_err().to_string().contains("request does not match"));
    }

    #[test]
    fn completion_distinguishes_expected_faults_from_missing_or_extra_io() {
        let transport = single_exchange(vec![Reply::Timeout, Reply::Disconnected]);
        assert!(transport.verify_complete().is_err());
        transport.set_feature(&[1, 2]).unwrap();
        assert!(transport.get_feature(&mut [0; 1]).is_err());
        assert!(transport.verify_complete().is_err());
        assert!(transport.get_feature(&mut [0; 1]).is_err());
        transport.verify_complete().unwrap();
        assert!(transport.get_feature(&mut [0; 1]).is_err());
        assert!(transport.verify_complete().is_err());
    }

    #[test]
    fn input_replay_preserves_short_edges_and_refuses_truncation() {
        let recording = InputRecording { interface: 1, recipe: "vendor-input-v1".into(), duration_ms: 1,
            end: RecordingEnd::Deadline, samples: [0, 1, 0, 1].into_iter().enumerate().map(|(n, value)|
                InputSample { elapsed_us: n as u64, bytes: vec![5, value] }).collect() };
        let reader = RecordedInput::new(&recording).unwrap();
        assert!(reader.read(&mut [0; 1]).is_err());
        let mut buffer = [0; 2];
        let mut got = Vec::new();
        while reader.read(&mut buffer).unwrap().is_some() { got.push(buffer[1]); }
        assert_eq!(got, [0, 1, 0, 1]);
    }

    #[test]
    fn protocol_replay_checks_requests_channels_busy_replies_and_actual_lengths() {
        let transport = RecordedProtocol::new(ProtocolTape { schema: 1, vid: 0x1234, pid: 0x5678,
            exchanges: vec![Exchange { channel: Channel::Feature, request: vec![1, 2],
                replies: vec![Reply::Bytes(vec![1]), Reply::Bytes(vec![2, 3])] },
                Exchange { channel: Channel::OutputInput, request: vec![3], replies: vec![Reply::Timeout] }] }).unwrap();
        assert!(transport.set_feature(&[1, 3]).is_err());
        transport.set_feature(&[1, 2]).unwrap();
        assert!(transport.read_input(&mut [0; 4], 10).is_err());
        let mut buffer = [0xaa; 4];
        assert_eq!(transport.get_feature(&mut buffer).unwrap(), 1);
        assert_eq!(buffer, [1, 0xaa, 0xaa, 0xaa]);
        assert!(transport.write_output(&[3]).is_err());
        assert_eq!(transport.get_feature(&mut buffer).unwrap(), 2);
        transport.write_output(&[3]).unwrap();
        assert!(transport.read_input(&mut buffer, 10).unwrap_err().to_string().contains("timeout"));
        assert!(transport.drained().unwrap());
    }
}
