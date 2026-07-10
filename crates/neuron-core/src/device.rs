//! A live device: the control transport + its registry definition, with wire exec routed through
//! the device's protocol [`Dialect`](crate::dialect::Dialect) (razer_report today).

use crate::dialect::Dialect;
use crate::registry::{CommandSpec, DeviceDef};
use crate::transport::{self, DevicePath, Transport};
use anyhow::{bail, Context, Result};
use std::collections::{HashMap, HashSet};

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
                    && def.matches_control(i)
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
            // find_for_pipe (not find_by_pid): resolve the def that actually DRIVES this pipe, so on
            // a two-family pid each control pipe reaches the family that can frame it.
            if let Some(def) = reg.find_for_pipe(i) {
                if def.command(cmd).is_some() {
                    return Device::open_path(def.clone(), i.pid, &i.path);
                }
            }
        }
        bail!("no connected device supports '{cmd}'")
    }

    /// The SEMANTIC sibling of [`open_with_command`](Self::open_with_command): resolve the first
    /// connected device that exposes a [`Capability`](crate::registry::Capability), not a single
    /// literal command name. Some capabilities have MORE THAN ONE wire dialect — `SetBrightness` is
    /// satisfied by either the matrix top-level `set_brightness` command OR a legacy `[lighting]`
    /// block's brightness spec (the BlackWidow Chroma V2) — so resolving those by a single command
    /// name reintroduces the exact dialect leak [`DeviceDef::supports`](crate::registry::DeviceDef::supports)
    /// exists to prevent: a legacy board that CAN set brightness reads as "no such command" and is
    /// never selected. Route dual-dialect writes through here so the capability gate decides.
    pub fn open_with_capability(
        reg: &crate::registry::Registry,
        cap: crate::registry::Capability,
    ) -> Result<Self> {
        for i in &transport::enumerate()? {
            // find_for_pipe (not find_by_pid): the def that DRIVES this pipe answers the capability
            // question, so a two-family pid resolves each pipe to its own frameable family.
            if let Some(def) = reg.find_for_pipe(i) {
                if def.supports(cap) {
                    return Device::open_path(def.clone(), i.pid, &i.path);
                }
            }
        }
        bail!("no connected device supports {cap:?}")
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
    ///
    /// ROUTED through the device's protocol [`Dialect`](crate::dialect::Dialect) (the busy-poll
    /// loop now lives in `dialect::RazerDialect::exec`): frame bytes and poll discipline are
    /// byte-identical to the moved-out loop, pinned by the dialect's frame goldens.
    pub fn exec_dynamic_tx(
        &self,
        transaction_id: u8,
        class: u8,
        id: u8,
        size: u8,
        args: &[u8],
    ) -> Result<[u8; 80]> {
        self.dialect()?
            .exec(self.transport.as_ref(), transaction_id, class, id, size, args)
    }

    /// The wire-protocol family this device speaks. FAIL CLOSED on an unknown id (Finding 2): with a
    /// second family registered and `dialect` being persisted, USER-EDITABLE data, a typo or a stale
    /// auto file must NOT silently fall back to razer — that would emit razer-framed bytes at
    /// possibly-non-razer hardware. A def whose family we can't identify gets no bytes on the wire at
    /// all; the ACK'd exec path surfaces this error loudly, `send_lighting_fast` no-ops on it, and
    /// `matches_control` already refuses to select such a def — defense in depth.
    fn dialect(&self) -> Result<&'static dyn Dialect> {
        crate::dialect::by_id(&self.def.dialect).with_context(|| {
            format!(
                "def '{}' declares unknown dialect '{}' — refusing to frame bytes for it",
                self.def.name, self.def.dialect
            )
        })
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
    ///
    /// ROUTED through the device's protocol [`Dialect`](crate::dialect::Dialect): the
    /// build-frame/set/wait/drain body now lives in `dialect::RazerDialect::exec_fast`, which
    /// receives `self.def.stream_wait_us` as the wait discipline — byte-identical to before.
    pub fn send_lighting_fast(&self, rep: &crate::lighting::Report) {
        let size = rep.size.unwrap_or_else(|| rep.args.len().min(80) as u8);
        let tx = rep.tx.unwrap_or(self.def.transaction_id);
        // Fire-and-forget path: a silent no-op on a mistagged def is the SAFE failure (Finding 2).
        // The ACK'd `exec_dynamic_tx` surfaces the unknown-dialect error loudly, and `matches_control`
        // already refuses to select such a def — so reaching here at all means a def slipped through;
        // the right move is to put NOTHING on the wire rather than razer-frame it blindly.
        let Ok(dialect) = self.dialect() else {
            return;
        };
        dialect.exec_fast(
            self.transport.as_ref(),
            tx,
            rep.class,
            rep.id,
            size,
            &rep.args,
            self.def.stream_wait_us,
        );
    }

    /// Release this family's CUSTODY of the device back to firmware — the rest-state restore run at
    /// stream teardown and app exit (DIALECT-RND "Device-mode lifecycle"). Teardown surfaces call
    /// THIS, never a raw device-mode write: the release routes through the def's [`Dialect`], so a
    /// razer board returns its driver-mode lease (device_mode -> 0x00, re-enabling onboard buttons/FN
    /// + firmware wake-restore) while a HID++ (or any never-in-custody) family no-ops instead of
    /// receiving a razer-framed mode packet it would misread. FAIL CLOSED on an unknown dialect,
    /// exactly like [`exec_dynamic_tx`](Self::exec_dynamic_tx) — a def we can't identify gets no bytes.
    pub fn release_custody(&self) -> Result<()> {
        self.dialect()?
            .release_custody(self.transport.as_ref(), &self.def)
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

    /// Resolve-and-cache under an arbitrary cache KEY, opening via `resolve` on a miss. The key
    /// NAMESPACES the `by_command` map so command-name resolution (`open_for`) and capability
    /// resolution (`with_writable_cap`, key `"cap:…"`) share one cache without colliding: registry
    /// command names never contain ':', so a `"cap:…"` key can never alias a real command name.
    fn open_for_key(
        &mut self,
        key: &str,
        resolve: impl Fn(&crate::registry::Registry) -> Result<Device>,
    ) -> Result<&Device> {
        if !self.by_command.contains_key(key) {
            let dev = resolve(self.reg)?;
            self.by_command.insert(key.to_string(), dev);
        }
        Ok(self
            .by_command
            .get(key)
            .expect("device cache was just populated"))
    }

    /// Resolve and cache the first connected device exposing `cmd`.
    pub fn open_for(&mut self, cmd: &str) -> Result<&Device> {
        self.open_for_key(cmd, |reg| Device::open_with_command(reg, cmd))
    }

    /// Drop one cached command handle and its driver-mode memo. Use after a transport failure,
    /// because wireless sleep/replug can leave an open HID handle stale while enumeration can reopen
    /// the same logical device.
    pub fn invalidate_command(&mut self, cmd: &str) {
        if let Some(dev) = self.by_command.remove(cmd) {
            self.driver_ready.remove(&DeviceKey::from_device(&dev));
        }
    }

    /// Resolve a write-capable device (under a cache `key`, opened via `resolve`) and run the Razer
    /// driver-mode handshake once per physical device. The shared core of both the command-name and
    /// capability writable paths, so they get IDENTICAL cached-handle + driver-memo semantics.
    fn writable_for_key(
        &mut self,
        key: &str,
        resolve: impl Fn(&crate::registry::Registry) -> Result<Device>,
    ) -> Result<&Device> {
        let dk = {
            let dev = self.open_for_key(key, &resolve)?;
            DeviceKey::from_device(dev)
        };
        if self.driver_ready.insert(dk) {
            let dev = self.open_for_key(key, &resolve)?;
            crate::writes::ensure_driver(dev);
        }
        self.open_for_key(key, &resolve)
    }

    /// Resolve a write-capable device and run the Razer driver-mode handshake once per device.
    pub fn writable_for(&mut self, cmd: &str) -> Result<&Device> {
        self.writable_for_key(cmd, |reg| Device::open_with_command(reg, cmd))
    }

    /// Shared retry CORE behind [`with_writable`]: resolve-and-cache under `key` via `resolve`, run
    /// `op`; on failure, invalidate the cached handle (forcing a fresh `resolve` + a re-run
    /// `ensure_driver` handshake through `writable_for_key`) and retry `op` once, wrapping a second
    /// failure with the first failure's text so neither is lost. `with_writable` below is the
    /// production convenience that supplies `cmd` as both the cache key and the `open_with_command`
    /// resolver — byte-identical behavior, just factored so the `(key, resolve)` pair is an explicit
    /// seam: a test can supply a `resolve` that returns a fake `Device` over an in-memory `Transport`,
    /// exercising this exact retry contract without ever reaching `Device::open_with_command`'s
    /// `transport::enumerate()` (which would touch real hardware).
    fn with_writable_via<T>(
        &mut self,
        key: &str,
        resolve: impl Fn(&crate::registry::Registry) -> Result<Device>,
        mut op: impl FnMut(&Device) -> Result<T>,
    ) -> Result<T> {
        match self.writable_for_key(key, &resolve).and_then(&mut op) {
            Ok(v) => Ok(v),
            Err(first) => {
                self.invalidate_command(key);
                self.writable_for_key(key, &resolve).and_then(&mut op).with_context(|| {
                    format!("after reopening cached '{key}' handle; first failure: {first}")
                })
            }
        }
    }

    /// Run one writable operation, reopening/re-handshaking once if the cached handle failed.
    pub fn with_writable<T>(
        &mut self,
        cmd: &str,
        op: impl FnMut(&Device) -> Result<T>,
    ) -> Result<T> {
        self.with_writable_via(cmd, |reg| Device::open_with_command(reg, cmd), op)
    }

    /// Run one writable operation resolved by CAPABILITY, reopening/re-handshaking once if the
    /// cached handle failed. The capability sibling of [`with_writable`](Self::with_writable): for a
    /// capability with more than one wire dialect (today only `SetBrightness` — top-level command vs
    /// legacy lighting-block spec), selecting by a single command name would skip the boards that
    /// only speak the other dialect. Uses the same stale-handle retry as `with_writable`; the cache
    /// key `"cap:…"` can't collide with a real command name (registry names never contain ':').
    pub fn with_writable_cap<T>(
        &mut self,
        cap: crate::registry::Capability,
        mut op: impl FnMut(&Device) -> Result<T>,
    ) -> Result<T> {
        let key = format!("cap:{cap:?}");
        let resolve = move |reg: &crate::registry::Registry| Device::open_with_capability(reg, cap);
        match self.writable_for_key(&key, &resolve).and_then(&mut op) {
            Ok(v) => Ok(v),
            Err(first) => {
                self.invalidate_command(&key);
                self.writable_for_key(&key, &resolve)
                    .and_then(&mut op)
                    .with_context(|| {
                        format!("after reopening cached '{key}' handle; first failure: {first}")
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
    // The live stream-strategy probe below builds raw frames directly (bypassing the dialect
    // seam, deliberately — DIALECT-RND do-not-disturb list), so it needs the protocol vocabulary
    // and timing types the production impl no longer imports at module top.
    use crate::protocol::{Report, BUF_LEN};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    /// The exact resolution gap this capability path closes: the legacy BlackWidow has NO top-level
    /// `set_brightness` command (its brightness lives in the `[lighting]` block), so the command-name
    /// resolver (`open_with_command`) never selected it — yet it CAN set brightness. `open_with_capability`
    /// keys off the SAME `def.supports(cap)` predicate, which honors the lighting dialect. Pins that the
    /// board is SELECTED by the capability gate while the literal command name is absent. No hardware.
    #[test]
    fn blackwidow_resolves_setbrightness_by_capability_not_command() {
        use crate::registry::Capability;
        let bw: DeviceDef =
            toml::from_str(include_str!("../devices/razer-blackwidow-chroma-v2.toml")).unwrap();
        assert!(
            bw.command("set_brightness").is_none(),
            "legacy board has no top-level set_brightness command — command-name resolution misses it"
        );
        assert!(
            bw.supports(Capability::SetBrightness),
            "yet it CAN set brightness via the lighting block, so open_with_capability's predicate selects it"
        );
    }

    /// Finding 2 — fail closed. A def tagged with an unknown dialect id (a typo, or a stale
    /// user-editable auto file) must put NO bytes on the wire: the ACK'd path errors LOUDLY naming
    /// the dialect, the fire-and-forget path silently no-ops — neither falls back to razer framing at
    /// possibly-non-razer hardware. The mock transport panics on ANY I/O, so a regression to the old
    /// razer fallback is caught as a panic, not a silent wrong-bytes pass.
    #[test]
    fn unknown_dialect_fails_closed_and_puts_no_bytes_on_the_wire() {
        struct PanicOnIo;
        impl Transport for PanicOnIo {
            fn set_feature(&self, _buf: &[u8]) -> Result<()> {
                panic!("unknown-dialect def must put NO bytes on the wire")
            }
            fn get_feature(&self, _buf: &mut [u8]) -> Result<()> {
                panic!("unknown-dialect def must read NOTHING")
            }
        }
        let mut def: DeviceDef =
            toml::from_str(include_str!("../devices/razer-blackwidow-chroma-v2.toml")).unwrap();
        def.dialect = "nope".into(); // a family we can't identify
        let dev = Device {
            def,
            pid: 0x0221,
            transport: Box::new(PanicOnIo),
        };
        // ACK'd path: an Err that names the offending dialect, raised BEFORE any transport I/O.
        let err = dev
            .exec_dynamic_tx(0x1f, 0x04, 0x85, 0x07, &[])
            .expect_err("unknown dialect must fail closed, not razer-frame bytes");
        let msg = format!("{err}");
        assert!(
            msg.contains("nope") && msg.contains("unknown dialect"),
            "the error names the dialect and the refusal: {msg}"
        );
        // Fire-and-forget path: a pure no-op (PanicOnIo is never touched, so no panic).
        dev.send_lighting_fast(&crate::lighting::Report {
            class: 0x03,
            id: 0x00,
            args: vec![0, 0, 0],
            tx: None,
            size: None,
        });
        // Teardown restore path: `release_custody` fails closed the same way — an unknown family gets
        // NO mode write, so a stream/app-exit teardown surface can't razer-frame a device-mode packet
        // at possibly-non-razer hardware (the finding this hook exists to fix). Errors before any I/O.
        let err = dev
            .release_custody()
            .expect_err("unknown dialect release must fail closed, not razer-frame a mode write");
        let msg = format!("{err}");
        assert!(
            msg.contains("nope") && msg.contains("unknown dialect"),
            "the release error names the dialect and the refusal: {msg}"
        );
    }

    /// A fake `Transport` that can be flipped "dead" via a shared `AtomicBool` — the stale-HID-handle
    /// scenario `DeviceSession::invalidate_command` exists for (wireless sleep/replug leaves an open
    /// handle stale while enumeration can reopen the same logical device). While dead, `set_feature`
    /// and `get_feature` fail IMMEDIATELY (no busy-poll delay: `RazerDialect::exec`'s `t.set_feature(&out)?`
    /// propagates the error before ever entering the poll loop). While alive it behaves like
    /// `dialect::tests::SharedPipe`: `get_feature` echoes whatever `(class, id)` was last `set_feature`d
    /// with a SUCCESS status. `device_mode_sets` counts how many times a device-mode SET (class
    /// 0x00/id 0x04, `writes::ensure_driver`'s handshake write) actually landed while alive, so a test
    /// can pin "the handshake re-ran on the reopened device" on a plain counter instead of guessing at
    /// call counts.
    #[derive(Clone)]
    struct DiesOnce {
        dead: Arc<AtomicBool>,
        last: Arc<Mutex<(u8, u8)>>,
        device_mode_sets: Arc<AtomicUsize>,
    }

    impl Transport for DiesOnce {
        fn set_feature(&self, buf: &[u8]) -> Result<()> {
            if self.dead.load(Ordering::SeqCst) {
                bail!("stale handle: set_feature failed (simulated unplug/sleep)");
            }
            let (class, id) = (buf[7], buf[8]);
            *self.last.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = (class, id);
            if class == crate::writes::CLASS_DEVICE_MODE && id == crate::writes::ID_DEVICE_MODE_SET {
                self.device_mode_sets.fetch_add(1, Ordering::SeqCst);
            }
            Ok(())
        }

        fn get_feature(&self, buf: &mut [u8]) -> Result<()> {
            if self.dead.load(Ordering::SeqCst) {
                bail!("stale handle: get_feature failed (simulated unplug/sleep)");
            }
            let (class, id) = *self.last.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            let mut rep = Report::command(0x1F, class, id, 0);
            rep.status = 0x02; // Success
            let out = rep.to_buf();
            let n = buf.len().min(out.len());
            buf[..n].copy_from_slice(&out[..n]);
            Ok(())
        }
    }

    /// TDD §8's named gap: "a test that `DeviceSession::with_writable` invalidates and retries a
    /// stale handle once." Drives the REAL retry code through the `with_writable_via` seam (no
    /// `transport::enumerate()`, no real hardware): attempt 1 resolves a fresh `Device`, ensure_driver's
    /// handshake succeeds (device is alive), then the write itself discovers the handle just went
    /// stale (models a wireless sleep landing between resolve and write) and fails. `with_writable`
    /// must invalidate, resolve a SECOND fresh handle, re-run the driver handshake on it, and retry the
    /// write — which this time succeeds because the "reopen" produced a healthy handle.
    #[test]
    fn with_writable_retries_once_recovers_from_a_stale_handle_and_rehandshakes() {
        let bw: DeviceDef =
            toml::from_str(include_str!("../devices/razer-blackwidow-chroma-v2.toml")).unwrap();
        let reg = crate::registry::Registry { devices: Vec::new() };

        let dead = Arc::new(AtomicBool::new(false)); // starts alive
        let device_mode_sets = Arc::new(AtomicUsize::new(0));
        let resolve_calls = Arc::new(AtomicUsize::new(0));
        let op_calls = Arc::new(AtomicUsize::new(0));

        let resolve = {
            let bw = bw.clone();
            let dead = dead.clone();
            let device_mode_sets = device_mode_sets.clone();
            let resolve_calls = resolve_calls.clone();
            move |_reg: &crate::registry::Registry| -> Result<Device> {
                let n = resolve_calls.fetch_add(1, Ordering::SeqCst) + 1;
                if n == 2 {
                    // The "reopen" produces a healthy handle — exactly what a real replug/wake does.
                    dead.store(false, Ordering::SeqCst);
                }
                Ok(Device {
                    def: bw.clone(),
                    pid: 0x0221,
                    transport: Box::new(DiesOnce {
                        dead: dead.clone(),
                        last: Arc::new(Mutex::new((0u8, 0u8))),
                        device_mode_sets: device_mode_sets.clone(),
                    }),
                })
            }
        };

        let mut session = DeviceSession::new(&reg);
        let out = {
            let op_calls = op_calls.clone();
            let dead = dead.clone();
            session.with_writable_via("test-write", resolve, move |d: &Device| {
                let n = op_calls.fetch_add(1, Ordering::SeqCst) + 1;
                if n == 1 {
                    // The handle goes stale right at write-time — the exact race the retry exists for.
                    dead.store(true, Ordering::SeqCst);
                }
                d.exec_dynamic(0x0c, 0x02, 0x01, &[9])
            })
        };

        assert!(
            out.is_ok(),
            "the retry must recover and propagate the second attempt's success: {out:?}"
        );
        assert_eq!(op_calls.load(Ordering::SeqCst), 2, "the write must be attempted exactly twice");
        assert_eq!(
            resolve_calls.load(Ordering::SeqCst),
            2,
            "resolve must run twice — a fresh handle each time, never a memoized stale one"
        );
        assert_eq!(
            device_mode_sets.load(Ordering::SeqCst),
            2,
            "ensure_driver's device-mode handshake must re-run on the reopened device (once per \
             resolved handle), not be skipped on retry"
        );
    }

    /// The other half of the retry contract: when the RE-OPENED handle is *also* dead (a genuinely
    /// unplugged device, not just a transient sleep), `with_writable` must still surface a single,
    /// informative error — and that error must retain the FIRST failure's text, not just the second's,
    /// so a diagnosing human sees the original symptom instead of only "retry also failed".
    #[test]
    fn with_writable_keeps_the_first_failure_in_context_when_the_reopen_also_fails() {
        let bw: DeviceDef =
            toml::from_str(include_str!("../devices/razer-blackwidow-chroma-v2.toml")).unwrap();
        let reg = crate::registry::Registry { devices: Vec::new() };

        let dead = Arc::new(AtomicBool::new(true)); // never recovers
        let device_mode_sets = Arc::new(AtomicUsize::new(0));
        let resolve_calls = Arc::new(AtomicUsize::new(0));

        let resolve = {
            let bw = bw.clone();
            let dead = dead.clone();
            let device_mode_sets = device_mode_sets.clone();
            let resolve_calls = resolve_calls.clone();
            move |_reg: &crate::registry::Registry| -> Result<Device> {
                resolve_calls.fetch_add(1, Ordering::SeqCst);
                Ok(Device {
                    def: bw.clone(),
                    pid: 0x0221,
                    transport: Box::new(DiesOnce {
                        dead: dead.clone(),
                        last: Arc::new(Mutex::new((0u8, 0u8))),
                        device_mode_sets: device_mode_sets.clone(),
                    }),
                })
            }
        };

        let mut session = DeviceSession::new(&reg);
        let err = session
            .with_writable_via("test-write", resolve, |d: &Device| {
                d.exec_dynamic(0x0c, 0x02, 0x01, &[9])
            })
            .expect_err("a permanently dead transport must fail even after the retry");

        assert_eq!(
            resolve_calls.load(Ordering::SeqCst),
            2,
            "the retry still resolves a fresh handle even though it too is dead"
        );
        let msg = format!("{err}");
        assert!(
            msg.contains("after reopening cached"),
            "the final error names the retry context: {msg}"
        );
        assert!(
            msg.contains("stale handle"),
            "the FIRST failure's text must survive into the final error, not just the second's: {msg}"
        );
    }

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
