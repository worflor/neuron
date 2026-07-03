//! A live device: the control transport + its registry definition + the busy-poll exec().

use crate::protocol::{Report, Status, BUF_LEN};
use crate::registry::{CommandSpec, DeviceDef};
use crate::transport::{self, DevicePath, Transport};
use anyhow::{bail, Context, Result};
use std::collections::{HashMap, HashSet};
use std::time::Duration;

pub struct Device {
    pub def: DeviceDef,
    pub pid: u16,
    transport: Box<dyn Transport>,
}

impl Device {
    /// Open a specific enumerated control interface path for `(def, pid)`.
    pub fn open_path(def: DeviceDef, pid: u16, path: &DevicePath) -> Result<Self> {
        let transport = transport::open_path(path)?;
        Ok(Device {
            def,
            pid,
            transport,
        })
    }

    /// Find the control interface for (def, pid) among enumerated HID collections and open it.
    pub fn open(def: DeviceDef, pid: u16) -> Result<Self> {
        let infos = transport::enumerate()?;
        let info = infos
            .into_iter()
            .find(|i| {
                i.vid == def.vendor_id
                    && i.pid == pid
                    && def.matches_control(i.usage_page, i.usage, i.feature_len)
            })
            .context("control interface not found (is the device connected?)")?;
        Device::open_path(def, pid, &info.path)
    }

    /// Open the FIRST connected, recognized device whose registry def exposes the named registry
    /// `cmd` — the shared "which device has this capability?" resolver. Both the CLI and the GUI
    /// dispatch use this so there is ONE device-resolution path (no per-client re-implementation
    /// that could disagree on the gate). For a WRITE intent, pass the SETTER command name (e.g.
    /// `"set_dpi"`), not the reader, so the predicate matches the operation.
    pub fn open_with_command(reg: &crate::registry::Registry, cmd: &str) -> Result<Self> {
        for i in &transport::enumerate()? {
            if let Some(def) = reg.find_by_pid(i.vid, i.pid) {
                if def.matches_control(i.usage_page, i.usage, i.feature_len)
                    && def.command(cmd).is_some()
                {
                    return Device::open_path(def.clone(), i.pid, &i.path);
                }
            }
        }
        bail!("no connected device supports '{cmd}'")
    }

    /// Send a command, then busy-poll for the echoed reply until SUCCESS (or terminal error).
    /// Returns the 80-byte argument payload. Read-only commands are non-mutating.
    pub fn exec(&self, cmd: &CommandSpec) -> Result<[u8; 80]> {
        self.exec_dynamic(cmd.class, cmd.id, cmd.size, &cmd.args)
    }

    /// Like `exec` but with caller-supplied class/id/size/args — needed for lighting, whose
    /// arguments (effect-id, colour, frame data) are computed at runtime, not fixed in TOML.
    /// SETTERS go through here; callers gate writes.
    pub fn exec_dynamic(&self, class: u8, id: u8, size: u8, args: &[u8]) -> Result<[u8; 80]> {
        self.exec_dynamic_tx(self.def.transaction_id, class, id, size, args)
    }

    /// Like [`exec_dynamic`](Self::exec_dynamic) but with an explicit `transaction_id`, for the
    /// (few) command families a device wires to a non-default tx — e.g. the Chroma V2's lighting
    /// EFFECT/CUSTOM-FRAME writes need 0x3F while its getters/brightness use the device default.
    /// `exec_dynamic` delegates here with `self.def.transaction_id`, so every device that sets no
    /// per-command override is byte-identical to before.
    pub fn exec_dynamic_tx(
        &self,
        transaction_id: u8,
        class: u8,
        id: u8,
        size: u8,
        args: &[u8],
    ) -> Result<[u8; 80]> {
        let mut req = Report::command(transaction_id, class, id, size);
        for (i, b) in args.iter().enumerate() {
            if i < req.args.len() {
                req.args[i] = *b;
            }
        }
        let cmd_class = class;
        let cmd_id = id;
        let out = req.to_buf();
        self.transport.set_feature(&out)?;
        for i in 0..60 {
            std::thread::sleep(Duration::from_millis(10));
            let mut b = [0u8; BUF_LEN];
            b[0] = 0x00; // report id for the GET
            if self.transport.get_feature(&mut b).is_ok() {
                // accept only a reply that echoes our class/id (filters cross-talk)
                if b[7] == cmd_class && b[8] == cmd_id {
                    match Status::from_u8(b[1]) {
                        Status::Success => return Ok(Report::from_buf(&b).args),
                        Status::Fail => {
                            bail!("device reported FAIL for command {cmd_class:#04x}/{cmd_id:#04x}")
                        }
                        Status::Unsupported => {
                            bail!("command {cmd_class:#04x}/{cmd_id:#04x} unsupported")
                        }
                        _ => {} // busy / timeout / new — keep polling
                    }
                }
            }
            if i % 12 == 11 {
                // re-arm if the device stayed busy (wireless round-trip can be slow)
                self.transport.set_feature(&out)?;
            }
        }
        bail!("timed out waiting for reply to {cmd_class:#04x}/{cmd_id:#04x}")
    }

    /// Apply a built lighting command (gated write path). Sends the dynamic-arg report and
    /// waits for the device's SUCCESS ack. The arg count is the protocol data-size.
    pub fn apply_lighting(&self, rep: &crate::lighting::Report) -> Result<()> {
        // data_size: prefer the report's explicit size (legacy class-0x03 commands need
        // OpenRazer's FIXED value, e.g. custom-frame 0x46) — else derive it from the arg
        // count (the matrix path, byte-identical to before).
        let size = rep.size.unwrap_or_else(|| rep.args.len().min(80) as u8);
        let tx = rep.tx.unwrap_or(self.def.transaction_id);
        self.exec_dynamic_tx(tx, rep.class, rep.id, size, &rep.args)?;
        Ok(())
    }

    /// Fast streaming write: send the report, wait the device's round-trip, then drain the reply ONCE
    /// (no busy-poll retry loop). The razer_report protocol requires the response to be read before the
    /// next command — skip it and the device ignores every subsequent write (frozen frame). It ALSO
    /// requires giving the link time to carry the command: on a 2.4 GHz dongle the SET round-trips
    /// host→dongle→mouse→back, and reading/firing again before that completes overruns the device and
    /// DROPS frames — the lighting FLICKER. `stream_wait_us` (registry data; ~31ms for the Naga's
    /// wireless receiver, 0 for wired boards) is exactly OpenRazer's per-receiver `wait_us`. Wired/legacy
    /// boards keep their ~1-2ms cost; the wireless mouse trades a lower ceiling (~16fps) for stability.
    pub fn send_lighting_fast(&self, rep: &crate::lighting::Report) {
        let size = rep.size.unwrap_or_else(|| rep.args.len().min(80) as u8);
        let tx = rep.tx.unwrap_or(self.def.transaction_id);
        let mut req = Report::command(tx, rep.class, rep.id, size);
        for (i, b) in rep.args.iter().enumerate() {
            if i < req.args.len() {
                req.args[i] = *b;
            }
        }
        if self.transport.set_feature(&req.to_buf()).is_ok() {
            if self.def.stream_wait_us > 0 {
                std::thread::sleep(Duration::from_micros(self.def.stream_wait_us));
            }
            let mut b = [0u8; BUF_LEN];
            let _ = self.transport.get_feature(&mut b); // drain the reply; don't busy-retry
        }
    }

    /// Run a named command from the device's registry command map.
    pub fn run(&self, name: &str) -> Result<[u8; 80]> {
        let cmd = self
            .def
            .command(name)
            .with_context(|| format!("device '{}' has no command '{}'", self.def.name, name))?;
        self.exec(cmd)
    }

    /// Run a named command with caller-supplied argument bytes (codes stay in the registry,
    /// values come from the call). For setters whose payload is computed at runtime.
    pub fn run_args(&self, name: &str, args: &[u8]) -> Result<[u8; 80]> {
        let cmd = self
            .def
            .command(name)
            .with_context(|| format!("device '{}' has no command '{}'", self.def.name, name))?;
        self.exec_dynamic(cmd.class, cmd.id, cmd.size, args)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct DeviceKey {
    vendor_id: u16,
    product_id: u16,
    name: String,
}

impl DeviceKey {
    fn from_device(d: &Device) -> Self {
        DeviceKey {
            vendor_id: d.def.vendor_id,
            product_id: d.pid,
            name: d.def.name.clone(),
        }
    }
}

/// A small live-session resolver for repeated device operations.
///
/// The hot dispatch loops ask for the same capability many times (DPI set/cycle, scroll, profile
/// apply). `Device::open_with_command` is the right one-shot API, but it enumerates HID and opens a
/// handle every call. `DeviceSession` keeps the resolved control pipe for the duration of a live
/// runtime or command burst, and memoizes the driver-mode handshake per physical device.
pub struct DeviceSession<'a> {
    reg: &'a crate::registry::Registry,
    by_command: HashMap<String, Device>,
    driver_ready: HashSet<DeviceKey>,
}

impl<'a> DeviceSession<'a> {
    pub fn new(reg: &'a crate::registry::Registry) -> Self {
        DeviceSession {
            reg,
            by_command: HashMap::new(),
            driver_ready: HashSet::new(),
        }
    }

    /// Drop cached handles and driver-mode memory. Use after a config/device topology reload.
    pub fn clear(&mut self) {
        self.by_command.clear();
        self.driver_ready.clear();
    }

    pub fn registry(&self) -> &'a crate::registry::Registry {
        self.reg
    }

    /// Resolve and cache the first connected device exposing `cmd`.
    pub fn open_for(&mut self, cmd: &str) -> Result<&Device> {
        if !self.by_command.contains_key(cmd) {
            let dev = Device::open_with_command(self.reg, cmd)?;
            self.by_command.insert(cmd.to_string(), dev);
        }
        Ok(self
            .by_command
            .get(cmd)
            .expect("device cache was just populated"))
    }

    /// Drop one cached command handle and its driver-mode memo. Use after a transport failure,
    /// because wireless sleep/replug can leave an open HID handle stale while enumeration can reopen
    /// the same logical device.
    pub fn invalidate_command(&mut self, cmd: &str) {
        if let Some(dev) = self.by_command.remove(cmd) {
            self.driver_ready.remove(&DeviceKey::from_device(&dev));
        }
    }

    /// Resolve a write-capable device and run the Razer driver-mode handshake once per device.
    pub fn writable_for(&mut self, cmd: &str) -> Result<&Device> {
        let key = {
            let dev = self.open_for(cmd)?;
            DeviceKey::from_device(dev)
        };
        if self.driver_ready.insert(key) {
            let dev = self.open_for(cmd)?;
            crate::writes::ensure_driver(dev);
        }
        self.open_for(cmd)
    }

    /// Run one writable operation, reopening/re-handshaking once if the cached handle failed.
    pub fn with_writable<T>(
        &mut self,
        cmd: &str,
        mut op: impl FnMut(&Device) -> Result<T>,
    ) -> Result<T> {
        match self.writable_for(cmd).and_then(&mut op) {
            Ok(v) => Ok(v),
            Err(first) => {
                self.invalidate_command(cmd);
                self.writable_for(cmd).and_then(&mut op).with_context(|| {
                    format!("after reopening cached '{cmd}' handle; first failure: {first}")
                })
            }
        }
    }

    /// Run one operation against a cached command-capable device, without forcing driver mode.
    ///
    /// Some higher-level write paths intentionally do their own gating and driver handshake after
    /// cheap preflight checks. This keeps those default/gated paths lightweight while still giving
    /// live sessions the same stale-handle recovery as `with_writable`.
    pub fn with_command<T>(
        &mut self,
        cmd: &str,
        mut op: impl FnMut(&Device) -> Result<T>,
    ) -> Result<T> {
        match self.open_for(cmd).and_then(&mut op) {
            Ok(v) => Ok(v),
            Err(first) => {
                self.invalidate_command(cmd);
                self.open_for(cmd).and_then(&mut op).with_context(|| {
                    format!("after reopening cached '{cmd}' handle; first failure: {first}")
                })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// LIVE stream-strategy probe — quantifies what one custom-frame report costs on the wire under
    /// four different SET/GET disciplines, on the real BlackWidow. Run with the app STOPPED (two
    /// writers on one control pipe corrupt both):
    /// `cargo test -p neuron --lib device::tests::live_stream_strategy_probe -- --ignored --nocapture`
    ///
    /// Background: the stream path (`send_lighting_fast`) does SetFeature + an IMMEDIATE GetFeature
    /// drain (wired `stream_wait_us` = 0). Razer firmware needs ~600-900µs to process a command
    /// before it can answer; a too-early GetFeature can stall the control pipe for milliseconds.
    /// A frame on this board = 7 reports, so per-report waste × 14 transfers decides the real fps
    /// ceiling — Synapse animates this same board far faster than the ~6fps we HISTORICALLY
    /// believed was the hardware limit (this probe falsified that: 30fps sustained clean under
    /// every strategy; the 6 came from the ACK'd path's 10ms poll). Each strategy streams a visible column
    /// chase at a 30fps target and reports achieved fps + per-call latency; watch the board for
    /// freezes/stutter, and the ACK'd round-trip after each strategy verifies the device survived.
    #[test]
    #[ignore = "live HID probe — BlackWidow attached, neuron-app stopped; run with --nocapture"]
    fn live_stream_strategy_probe() {
        use std::time::Instant;
        let reg = crate::registry::Registry::load().expect("registry loads");
        let Some(def) = reg.find_by_pid(0x1532, 0x0221) else {
            eprintln!("skip: no BlackWidow def in the registry");
            return;
        };
        let Ok(dev) = Device::open(def.clone(), 0x0221) else {
            eprintln!("skip: BlackWidow not connected");
            return;
        };
        let ldef = def.lighting.clone().expect("keyboard def has lighting");
        let (rows, cols) = (ldef.rows as usize, ldef.cols as usize);
        let display = ldef.custom_display_report();

        // serialize a lighting Report to the raw 90-byte buffer exactly like send_lighting_fast
        let raw = |rep: &crate::lighting::Report| -> Vec<u8> {
            let size = rep.size.unwrap_or_else(|| rep.args.len().min(80) as u8);
            let tx = rep.tx.unwrap_or(dev.def.transaction_id);
            let mut req = Report::command(tx, rep.class, rep.id, size);
            for (i, b) in rep.args.iter().enumerate() {
                if i < req.args.len() {
                    req.args[i] = *b;
                }
            }
            req.to_buf().to_vec()
        };

        // a bright column chase — dropped or frozen frames read as visible stutter on the board
        let frame_at = |k: usize| -> Vec<crate::lighting::Rgb> {
            (0..rows * cols)
                .map(|i| {
                    if i % cols == k % cols {
                        crate::lighting::Rgb::new(0, 255, 140)
                    } else {
                        crate::lighting::Rgb::new(6, 0, 24)
                    }
                })
                .collect()
        };

        const FRAMES: usize = 90; // 3s at the 30fps target
        let budget = Duration::from_millis(33);
        let ms = |ns: u128| ns as f64 / 1e6;
        for (name, gap_us, drain) in [
            ("A  set + get, no gap (CURRENT)", 0u64, true),
            ("B  set + 800us gap + get      ", 800, true),
            ("C  set + 900us gap, NO get    ", 900, false),
            ("D  set only, no gap, no get   ", 0, false),
        ] {
            let (mut set_ns, mut get_ns) = (Vec::new(), Vec::new());
            let (mut set_fail, mut get_fail, mut overruns) = (0u32, 0u32, 0u32);
            let t_run = Instant::now();
            let mut next = Instant::now();
            for k in 0..FRAMES {
                let f0 = Instant::now();
                let frame = frame_at(k);
                let mut bufs: Vec<Vec<u8>> = (0..rows)
                    .filter_map(|r| ldef.row_report(&frame, r))
                    .map(|r| raw(&r))
                    .collect();
                bufs.push(raw(&display));
                for buf in &bufs {
                    let t = Instant::now();
                    if dev.transport.set_feature(buf).is_err() {
                        set_fail += 1;
                    }
                    set_ns.push(t.elapsed().as_nanos());
                    if gap_us > 0 {
                        std::thread::sleep(Duration::from_micros(gap_us));
                    }
                    if drain {
                        let t = Instant::now();
                        let mut b = [0u8; BUF_LEN];
                        if dev.transport.get_feature(&mut b).is_err() {
                            get_fail += 1;
                        }
                        get_ns.push(t.elapsed().as_nanos());
                    }
                }
                if f0.elapsed() > budget {
                    overruns += 1;
                }
                next += budget;
                let now = Instant::now();
                if next > now {
                    std::thread::sleep(next - now);
                } else {
                    next = now; // overran — don't burst to catch up
                }
            }
            let achieved = FRAMES as f64 / t_run.elapsed().as_secs_f64();
            let stats = |v: &mut Vec<u128>| -> (f64, f64, f64) {
                if v.is_empty() {
                    return (0.0, 0.0, 0.0);
                }
                v.sort_unstable();
                let avg = v.iter().sum::<u128>() as f64 / v.len() as f64 / 1e6;
                (avg, ms(v[v.len() * 95 / 100]), ms(*v.last().unwrap()))
            };
            let (sa, sp, sm) = stats(&mut set_ns);
            let (ga, gp, gm) = stats(&mut get_ns);
            // aliveness: an ACK'd round-trip must still succeed (a wedged protocol would fail here)
            let alive = dev
                .exec_dynamic_tx(
                    display.tx.unwrap_or(dev.def.transaction_id),
                    display.class,
                    display.id,
                    display.size.unwrap_or(display.args.len() as u8),
                    &display.args,
                )
                .is_ok();
            println!(
                "{name} | {achieved:5.1} fps (target 30) | set avg/p95/max {sa:.2}/{sp:.2}/{sm:.2} ms ({set_fail} fail) | get avg/p95/max {ga:.2}/{gp:.2}/{gm:.2} ms ({get_fail} fail) | overruns {overruns}/{FRAMES} | alive-after {alive}"
            );
            std::thread::sleep(Duration::from_millis(400));
        }
    }
}
