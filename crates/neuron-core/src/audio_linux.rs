// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

//! PulseAudio protocol control through pactl/parec. PipeWire's PulseAudio compatibility server
//! exposes the same interface. Commands receive fixed argv fields, never shell-interpreted text.

use super::{Endpoint, Flow};
use serde_json::Value;
use std::io::Read;
use std::process::{Child, Command, Stdio};
use std::sync::{mpsc, Mutex};

fn pactl(args: &[&str]) -> Option<String> {
    let output = Command::new("pactl").args(args).output().ok()?;
    output.status.success().then(|| String::from_utf8(output.stdout).ok()).flatten()
}

fn list(flow: Flow) -> Vec<Value> {
    let group = match flow { Flow::Render => "sinks", Flow::Capture => "sources" };
    pactl(&["--format=json", "list", group])
        .and_then(|text| serde_json::from_str::<Vec<Value>>(&text).ok())
        .unwrap_or_default()
}

fn scalar(value: &Value) -> f32 {
    let Some(channels) = value.get("volume").and_then(Value::as_object) else { return 0.0 };
    let mut sum = 0.0;
    let mut count = 0.0;
    for channel in channels.values() {
        if let Some(raw) = channel.get("value").and_then(Value::as_f64) {
            sum += raw / 65536.0;
            count += 1.0;
        }
    }
    if count > 0.0 { (sum / count).clamp(0.0, 1.0) as f32 } else { 0.0 }
}

fn endpoint(value: &Value, flow: Flow) -> Option<Endpoint> {
    let id = value.get("name")?.as_str()?.to_string();
    // Monitor sources are the output loopback path, not microphones offered in the capture picker.
    if flow == Flow::Capture && value.get("properties")
        .and_then(|p| p.get("device.class"))
        .and_then(Value::as_str) == Some("monitor") {
        return None;
    }
    let name = value.get("description").and_then(Value::as_str).unwrap_or(&id).to_string();
    Some(Endpoint {
        id, name, flow, volume: scalar(value),
        muted: value.get("mute").and_then(Value::as_bool).unwrap_or(false),
    })
}

pub fn endpoints(flow: Flow) -> Vec<Endpoint> {
    list(flow).iter().filter_map(|value| endpoint(value, flow)).collect()
}

pub struct VolumeCtl {
    id: String,
    flow: Flow,
}

impl VolumeCtl {
    pub fn open(id: &str) -> Option<Self> {
        for flow in [Flow::Render, Flow::Capture] {
            if endpoints(flow).iter().any(|endpoint| endpoint.id == id) {
                return Some(Self { id: id.into(), flow });
            }
        }
        None
    }

    fn current(&self) -> Option<Endpoint> {
        endpoints(self.flow).into_iter().find(|endpoint| endpoint.id == self.id)
    }

    fn noun(&self) -> &'static str {
        match self.flow { Flow::Render => "sink", Flow::Capture => "source" }
    }

    pub fn get_volume(&self) -> f32 { self.current().map_or(0.0, |endpoint| endpoint.volume) }

    pub fn set_volume(&self, scalar: f32) -> bool {
        if !scalar.is_finite() { return false; }
        let percent = format!("{}%", (scalar.clamp(0.0, 1.0) * 100.0).round() as u32);
        let command = format!("set-{}-volume", self.noun());
        pactl(&[&command, &self.id, &percent]).is_some()
            && self.current().is_some_and(|endpoint| (endpoint.volume - scalar.clamp(0.0, 1.0)).abs() <= 0.02)
    }

    pub fn nudge(&self, delta: f32) -> f32 {
        let target = (self.get_volume() + delta).clamp(0.0, 1.0);
        let _ = self.set_volume(target);
        self.get_volume()
    }

    pub fn get_mute(&self) -> bool { self.current().is_some_and(|endpoint| endpoint.muted) }

    pub fn set_mute(&self, mute: bool) -> bool {
        if self.current().is_some_and(|endpoint| endpoint.muted == mute) { return false; }
        let command = format!("set-{}-mute", self.noun());
        pactl(&[&command, &self.id, if mute { "1" } else { "0" }]).is_some()
            && self.current().is_some_and(|endpoint| endpoint.muted == mute)
    }

    pub fn toggle_mute(&self) -> bool {
        let next = !self.get_mute();
        self.set_mute(next);
        next
    }
}

pub fn find_capture(needle: &str) -> Option<Endpoint> {
    endpoints(Flow::Capture).into_iter()
        .find(|endpoint| super::endpoint_matches_product(&endpoint.name, needle))
}

pub fn resolve_capture(device: Option<&str>) -> Option<Endpoint> {
    let all = endpoints(Flow::Capture);
    super::pick_by_needles(&all, super::explicit_needle(device), &[]).cloned()
}

pub fn find_render(needle: &str) -> Option<Endpoint> {
    endpoints(Flow::Render).into_iter()
        .find(|endpoint| super::endpoint_matches_product(&endpoint.name, needle))
}

pub fn resolve_render(device: Option<&str>) -> Option<Endpoint> {
    let all = endpoints(Flow::Render);
    super::pick_render(&all, super::explicit_needle(device), default_render_id().as_deref()).cloned()
}

pub fn default_render_id() -> Option<String> {
    pactl(&["get-default-sink"]).map(|id| id.trim().to_string()).filter(|id| !id.is_empty())
}

pub fn flip_candidates(names: &[String]) -> Vec<Endpoint> {
    let all = endpoints(Flow::Render);
    if names.is_empty() { return all; }
    names.iter().filter_map(|name| all.iter()
        .find(|endpoint| super::endpoint_matches_product(&endpoint.name, name)).cloned())
        .collect()
}

pub fn set_default(id: &str) -> bool {
    pactl(&["set-default-sink", id]).is_some()
        && default_render_id().as_deref() == Some(id)
}

pub fn flip_output(names: &[String]) -> String {
    let candidates = flip_candidates(names);
    if candidates.is_empty() { return "no output devices connected".into(); }
    let current = default_render_id();
    let index = current.as_deref().and_then(|id| candidates.iter().position(|e| e.id == id));
    let next = &candidates[index.map_or(0, |index| (index + 1) % candidates.len())];
    if set_default(&next.id) {
        format!("output → {}", next.name)
    } else {
        format!("output flip failed ({})", next.name)
    }
}

/// A nonblocking mono PCM stream. A reader thread owns the blocking parec stdout; the analysis
/// tick drains already-buffered samples and can notice a dead source without ever waiting on it.
pub struct CaptureCtl {
    child: Mutex<Child>,
    rx: Mutex<mpsc::Receiver<Vec<f32>>>,
    rate: u32,
}

impl CaptureCtl {
    fn open(id: &str) -> Option<Self> {
        let mut child = Command::new("parec")
            .args(["--raw", "--format=float32le", "--channels=1", "--rate=48000", "--device", id])
            .stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn().ok()?;
        let mut stdout = child.stdout.take()?;
        let (tx, rx) = mpsc::sync_channel(16);
        if !crate::worker::spawn_detached("neuron-audio-capture", move || {
            let mut raw = [0u8; 4096];
            let mut pending = Vec::<u8>::new();
            while let Ok(n) = stdout.read(&mut raw) {
                if n == 0 { break; }
                pending.extend_from_slice(&raw[..n]);
                let complete = pending.len() & !3;
                if complete == 0 { continue; }
                let samples: Vec<f32> = pending[..complete].chunks_exact(4)
                    .map(|bytes| f32::from_le_bytes(bytes.try_into().expect("four bytes")))
                    .collect();
                pending.drain(..complete);
                match tx.try_send(samples) {
                    Ok(()) | Err(mpsc::TrySendError::Full(_)) => {}
                    Err(mpsc::TrySendError::Disconnected(_)) => break,
                }
            }
        }) {
            let _ = child.kill();
            let _ = child.wait();
            return None;
        }
        Some(Self { child: Mutex::new(child), rx: Mutex::new(rx), rate: 48_000 })
    }

    pub fn open_loopback_default() -> Option<Self> {
        let sink = default_render_id()?;
        let monitor = list(Flow::Render).into_iter()
            .find(|value| value.get("name").and_then(Value::as_str) == Some(&sink))?
            .get("monitor_source")?.as_str()?.to_string();
        Self::open(&monitor)
    }

    pub fn open_capture(id: &str) -> Option<Self> {
        endpoints(Flow::Capture).iter().any(|source| source.id == id).then(|| Self::open(id)).flatten()
    }

    pub fn rate(&self) -> u32 { self.rate }

    pub fn read_into(&self, out: &mut Vec<f32>) -> Option<usize> {
        let rx = self.rx.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut appended = 0;
        loop {
            match rx.try_recv() {
                Ok(samples) => { appended += samples.len(); out.extend(samples); }
                Err(mpsc::TryRecvError::Empty) => return Some(appended),
                Err(mpsc::TryRecvError::Disconnected) => return None,
            }
        }
    }
}

impl Drop for CaptureCtl {
    fn drop(&mut self) {
        let mut child = self.child.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let _ = child.kill();
        let _ = child.wait();
    }
}

pub struct MeterCtl { capture: CaptureCtl }

impl MeterCtl {
    pub fn open_default_render() -> Option<Self> {
        Some(Self { capture: CaptureCtl::open_loopback_default()? })
    }
    pub fn open(id: &str) -> Option<Self> {
        let monitor = list(Flow::Render).into_iter()
            .find(|value| value.get("name").and_then(Value::as_str) == Some(id))?
            .get("monitor_source")?.as_str()?.to_string();
        Some(Self { capture: CaptureCtl::open(&monitor)? })
    }
    pub fn try_peak(&self) -> Option<f32> {
        let mut samples = Vec::new();
        self.capture.read_into(&mut samples)?;
        Some(samples.into_iter().map(f32::abs).fold(0.0f32, f32::max).min(1.0))
    }
    pub fn peak(&self) -> f32 { self.try_peak().unwrap_or(0.0) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoint_parser_recognizes_capture_and_monitor() {
        let input = serde_json::json!({"name":"mic", "description":"USB Mic", "volume":{"mono":{"value":32768}}, "mute":true, "properties":{"device.class":"sound"}});
        let parsed = endpoint(&input, Flow::Capture).unwrap();
        assert_eq!(parsed.id, "mic");
        assert_eq!(parsed.volume, 0.5);
        assert!(parsed.muted);
        let monitor = serde_json::json!({"name":"sink.monitor", "properties":{"device.class":"monitor"}});
        assert!(endpoint(&monitor, Flow::Capture).is_none());
    }
}
