// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

//! Phantom devices — a scriptable HID [`Backend`] that stands in for real hardware.
//!
//! ## Why this exists
//!
//! Every fake in this repo before now hardcoded exactly ONE behaviour: reply SUCCESS and echo
//! (`dialect::RecordingMock`), reply UNSUPPORTED for anything unlisted (`synth::MockDevice`),
//! panic on any I/O (`PanicOnIo`), or die once (`device::DiesOnce`). Not one of them could model a
//! NAK, garbage bytes, a bad CRC, a partial read, a silent pipe, or a mid-conversation unplug — so
//! not one line of neuron's failure handling was under test, even though the failure handling is
//! most of what the device layer *is*.
//!
//! That gap has teeth. `synth`'s probe loop collapses transport-error, NAK, UNSUPPORTED, and
//! timeout into a single `None` that all four call sites read as "this device lacks this
//! capability" — and that verdict is then frozen into `devices/auto/<pid>.toml` forever. A device
//! that merely dozed through the sweep is permanently under-reported. No test catches it because
//! no fake could doze.
//!
//! ## The parrot is real, and it is the reason for [`Answer::Echo`]
//!
//! `dialect.rs`'s razer-audio notes record a live finding: the Seiren V3 Mini answers
//! **SUCCESS with an all-zero body for ~485 of 512 unknown `(class, id)` headers.** A probe that
//! trusts the status byte alone synthesizes a fully-populated, fully-false capability map from
//! such a device. The response at the time was to disable probing for that one family; the
//! underlying lesson — *a status byte is not evidence, the payload must carry information* — was
//! never generalized. [`MockDevice::parroting`] makes that firmware reproducible on demand so the
//! validator can be written against it and kept honest.
//!
//! ## Shape
//!
//! [`MockDevice`] is the phantom's shared state; [`MockHandle`] is one opened handle onto it, and
//! [`MockDevice::handle`] can mint several. Two handles share both the state and the
//! [`WireLock`] — the same topology two real `CreateFileW` calls on one `DevicePath` produce,
//! which is what `dialect::SharedPipe` models and what the cross-read-reply guard needs.
//!
//! ## Ground truth, not self-agreement
//!
//! A mock that answers from the same table the code under test consults proves nothing — the
//! `hidpp.rs` suite has this problem in-tree (its scripted replies and its implementation are both
//! transcriptions of the same Solaar document, so the mock is its own oracle). Phantoms are for
//! *mechanism* — the failure modes, the state machine, the byte framing. For *vocabulary* — which
//! opcodes a real board actually answers — assert against a curated def or a recorded tape, the
//! way `synth.rs`'s suite already compares synthesis output to `devices/razer-naga-v2-pro.toml`.

use super::{Backend, DevicePath, HidDeviceInfo, InputReader, Transport, WireLock};
use crate::protocol::{Report, BUF_LEN};
use anyhow::{bail, Result};
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

/// How the phantom answers one `(class, id)` request.
#[derive(Clone, Debug)]
pub enum Answer {
    /// SUCCESS, echoing the request's class/id, carrying this body (zero-padded to 80 bytes).
    Success(Vec<u8>),
    /// A non-SUCCESS status byte, echoing class/id. See [`crate::protocol::Status`]:
    /// 0x01 busy, 0x03 fail, 0x04 timeout, 0x05 unsupported.
    Status(u8),
    /// SUCCESS with an **all-zero body** — the parrot. Structurally indistinguishable from a real
    /// answer to anything that only reads the status byte.
    Echo,
}

/// A firmware misbehaviour, consumed one per conversation from the phantom's fault queue.
#[derive(Clone, Debug)]
pub enum Fault {
    /// The request is accepted, but no reply ever echoes it. Drives a poll loop to its full
    /// timeout budget (the razer dialect spends 60 × 10 ms here).
    Silence,
    /// The reply echoes a DIFFERENT `(class, id)` than was asked — another conversation's answer
    /// cross-read off the same pipe. `reply_status` must reject it and keep polling.
    CrossTalk,
    /// The reply frame carries this body instead of the scripted one.
    Garbage(Vec<u8>),
    /// A structurally valid reply with a corrupted CRC byte. Nothing in neuron verifies receive
    /// CRC today — this fault is the test that will fail until something does.
    CrcCorrupt,
    /// `get_feature` fills only the first `n` bytes and leaves the rest untouched.
    ShortRead(usize),
    /// This call and **every** later one error: the device was unplugged mid-conversation.
    Yank,
    /// This one call errors; normal service resumes after it.
    Blip,
}

/// A phantom device: the state behind one or more [`MockHandle`]s.
pub struct MockDevice {
    /// What fake enumeration reports for this device.
    pub info: HidDeviceInfo,
    answers: HashMap<(u8, u8), Answer>,
    /// When set, any `(class, id)` with no explicit answer replies [`Answer::Echo`] rather than
    /// UNSUPPORTED — the Seiren's observed behaviour.
    parrot: bool,
    faults: Mutex<VecDeque<Fault>>,
    log: Mutex<Vec<Vec<u8>>>,
    /// The reply owed for the last request, plus a truncation length (0 = deliver in full).
    /// Truncation is tracked out-of-band rather than in a spare wire byte so the phantom's frames
    /// stay byte-identical to what real firmware would put on the pipe.
    pending: Mutex<Option<([u8; BUF_LEN], usize)>>,
    yanked: AtomicBool,
    wire: Arc<WireLock>,
}

impl MockDevice {
    /// A phantom shaped like a Razer `razer_report` control pipe: VID 0x1532, 91-byte feature
    /// report — the exact signature `RazerDialect::claims` matches on.
    pub fn razer(pid: u16, product: &str) -> Self {
        Self::new(HidDeviceInfo {
            vid: 0x1532,
            pid,
            usage_page: 0xFF00,
            usage: 0x0002,
            feature_len: BUF_LEN as u16,
            input_len: 0,
            output_len: 0,
            path: DevicePath::from_str_for_tests(&format!(
                "\\\\?\\hid#vid_1532&pid_{pid:04x}&mi_02#phantom"
            )),
            product: product.to_string(),
        })
    }

    /// A phantom with a caller-built enumeration record — for non-Razer shapes (a third-party
    /// keyboard, an output-report family, an unclaimed pipe).
    pub fn new(info: HidDeviceInfo) -> Self {
        MockDevice {
            info,
            answers: HashMap::new(),
            parrot: false,
            faults: Mutex::new(VecDeque::new()),
            log: Mutex::new(Vec::new()),
            pending: Mutex::new(None),
            yanked: AtomicBool::new(false),
            wire: Arc::new(WireLock::new_local()),
        }
    }

    /// This `(class, id)` answers SUCCESS carrying `args`.
    #[must_use]
    pub fn answering(mut self, class: u8, id: u8, args: &[u8]) -> Self {
        self.answers
            .insert((class, id), Answer::Success(args.to_vec()));
        self
    }

    /// This `(class, id)` answers with a non-SUCCESS status (default UNSUPPORTED elsewhere).
    #[must_use]
    pub fn refusing(mut self, class: u8, id: u8, status: u8) -> Self {
        self.answers.insert((class, id), Answer::Status(status));
        self
    }

    /// Answer SUCCESS-with-empty-body to every otherwise-unknown header — the parrot firmware
    /// observed on the Seiren V3 Mini. A probe that reads only the status byte will believe this
    /// device implements everything it is asked about.
    #[must_use]
    pub fn parroting(mut self) -> Self {
        self.parrot = true;
        self
    }

    /// Queue a misbehaviour. Faults are consumed in order, one per conversation.
    #[must_use]
    pub fn faulting(self, fault: Fault) -> Self {
        self.faults
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push_back(fault);
        self
    }

    /// Every request byte-buffer this phantom received, in order.
    pub fn log(&self) -> Vec<Vec<u8>> {
        self.log
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// The `(class, id)` of every request received, in order — the usual assertion shape.
    pub fn asked(&self) -> Vec<(u8, u8)> {
        self.log()
            .iter()
            .filter(|b| b.len() > 8)
            .map(|b| (b[7], b[8]))
            .collect()
    }

    /// Mint another handle onto this same phantom: shared state, shared [`WireLock`] — what two
    /// `CreateFileW` opens on one `DevicePath` produce.
    pub fn handle(self: &Arc<Self>) -> MockHandle {
        MockHandle(Arc::clone(self))
    }

    fn take_fault(&self) -> Option<Fault> {
        self.faults
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .pop_front()
    }

    /// Build the reply this phantom owes for `request`, or `None` to stay silent.
    fn reply_to(&self, request: &[u8]) -> Option<[u8; BUF_LEN]> {
        if request.len() <= 8 {
            return None;
        }
        let (class, id) = (request[7], request[8]);
        let answer = match self.answers.get(&(class, id)) {
            Some(a) => a.clone(),
            None if self.parrot => Answer::Echo,
            // 0x05 = UNSUPPORTED: the honest "I do not implement this" a real board gives.
            None => Answer::Status(0x05),
        };

        let mut report = Report::command(request[2], class, id, request[6]);
        match answer {
            Answer::Success(args) => {
                report.status = 0x02;
                let n = args.len().min(80);
                report.args[..n].copy_from_slice(&args[..n]);
            }
            Answer::Echo => report.status = 0x02,
            Answer::Status(s) => report.status = s,
        }
        Some(report.to_buf())
    }
}

/// One opened handle onto a [`MockDevice`].
pub struct MockHandle(Arc<MockDevice>);

impl MockHandle {
    /// The phantom behind this handle, for assertions.
    pub fn device(&self) -> &Arc<MockDevice> {
        &self.0
    }
}

impl Transport for MockHandle {
    fn set_feature(&self, buf: &[u8]) -> Result<()> {
        let d = &self.0;
        if d.yanked.load(Ordering::Relaxed) {
            bail!("mock: device was yanked");
        }
        d.log
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(buf.to_vec());

        let mut reply = d.reply_to(buf);
        let mut truncate = 0usize;
        match d.take_fault() {
            None => {}
            Some(Fault::Silence) => reply = None,
            Some(Fault::Blip) => bail!("mock: transient I/O failure"),
            Some(Fault::Yank) => {
                d.yanked.store(true, Ordering::Relaxed);
                bail!("mock: device yanked mid-conversation");
            }
            Some(Fault::CrossTalk) => {
                // Echo a header nobody asked for: the classic two-handles-on-one-pipe symptom.
                if let Some(r) = reply.as_mut() {
                    r[7] = r[7].wrapping_add(1);
                    r[8] = r[8].wrapping_add(1);
                }
            }
            Some(Fault::Garbage(bytes)) => {
                if let Some(r) = reply.as_mut() {
                    let n = bytes.len().min(80);
                    r[9..9 + n].copy_from_slice(&bytes[..n]);
                }
            }
            Some(Fault::CrcCorrupt) => {
                if let Some(r) = reply.as_mut() {
                    r[89] = r[89].wrapping_add(1);
                }
            }
            Some(Fault::ShortRead(n)) => truncate = n,
        }
        *d.pending.lock().unwrap_or_else(PoisonError::into_inner) = reply.map(|r| (r, truncate));
        Ok(())
    }

    fn get_feature(&self, buf: &mut [u8]) -> Result<usize> {
        let d = &self.0;
        if d.yanked.load(Ordering::Relaxed) {
            bail!("mock: device was yanked");
        }
        let pending = *d.pending.lock().unwrap_or_else(PoisonError::into_inner);
        let Some((reply, truncate)) = pending else {
            // Silence: leave the caller's buffer untouched and report ZERO bytes read. `reply_status`
            // will not match, so a poll loop keeps polling — exactly what a mute pipe does.
            return Ok(0);
        };
        let full = buf.len().min(BUF_LEN);
        let n = if truncate > 0 { truncate.min(full) } else { full };
        buf[..n].copy_from_slice(&reply[..n]);
        // The REAL count — this is what makes `Fault::ShortRead` observable to the dialects instead
        // of hiding behind the caller's zeroed buffer.
        Ok(n)
    }

    fn wire_lock(&self) -> Option<Arc<WireLock>> {
        Some(Arc::clone(&self.0.wire))
    }
}

/// A scripted stand-in for device-pushed input reports.
///
/// `Ok(Some(n))` per queued frame, then `Ok(None)` (idle) forever — never `Err`, so a listener
/// loop under test terminates on its own stop flag rather than on a synthetic device death.
/// Queue a final [`ReadStep::Gone`](super::ReadStep)-shaped ending by wrapping in [`Fault::Yank`]
/// on the paired [`MockDevice`] instead.
pub struct MockReader {
    frames: Mutex<VecDeque<Vec<u8>>>,
}

impl MockReader {
    pub fn new(frames: Vec<Vec<u8>>) -> Self {
        MockReader {
            frames: Mutex::new(frames.into()),
        }
    }
}

impl InputReader for MockReader {
    fn read(&self, buf: &mut [u8]) -> Result<Option<usize>> {
        let next = self
            .frames
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .pop_front();
        match next {
            Some(f) => {
                let n = f.len().min(buf.len());
                buf[..n].copy_from_slice(&f[..n]);
                Ok(Some(n))
            }
            None => Ok(None),
        }
    }
}

/// A [`Backend`] serving a fixed set of phantoms.
#[derive(Default)]
pub struct MockBackend {
    devices: Vec<Arc<MockDevice>>,
    pushes: Mutex<HashMap<String, Vec<Vec<u8>>>>,
}

impl MockBackend {
    pub fn new() -> Self {
        MockBackend::default()
    }

    /// Add a phantom and hand back the `Arc` so the test can assert against it afterwards.
    pub fn with(&mut self, device: MockDevice) -> Arc<MockDevice> {
        let d = Arc::new(device);
        self.devices.push(Arc::clone(&d));
        d
    }

    /// Script the input reports `open_reader` will replay for the phantom at `path`.
    pub fn pushing(&self, path: &DevicePath, frames: Vec<Vec<u8>>) {
        self.pushes
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(key(path), frames);
    }

    fn find(&self, path: &DevicePath) -> Option<&Arc<MockDevice>> {
        self.devices.iter().find(|d| d.info.path == *path)
    }
}

fn key(path: &DevicePath) -> String {
    format!("{path:?}")
}

impl Backend for MockBackend {
    fn enumerate(&self) -> Result<Vec<HidDeviceInfo>> {
        Ok(self
            .devices
            .iter()
            .map(|d| HidDeviceInfo {
                vid: d.info.vid,
                pid: d.info.pid,
                usage_page: d.info.usage_page,
                usage: d.info.usage,
                feature_len: d.info.feature_len,
                input_len: d.info.input_len,
                output_len: d.info.output_len,
                path: d.info.path.clone(),
                product: d.info.product.clone(),
            })
            .collect())
    }

    fn open_path(&self, path: &DevicePath) -> Result<Box<dyn Transport>> {
        match self.find(path) {
            Some(d) if !d.yanked.load(Ordering::Relaxed) => Ok(Box::new(d.handle())),
            Some(_) => bail!("mock: device at {path:?} is yanked"),
            None => bail!("mock: no phantom at {path:?}"),
        }
    }

    fn open_reader(&self, path: &DevicePath) -> Result<Box<dyn InputReader>> {
        if self.find(path).is_none() {
            bail!("mock: no phantom at {path:?}");
        }
        let frames = self
            .pushes
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&key(path))
            .cloned()
            .unwrap_or_default();
        Ok(Box::new(MockReader::new(frames)))
    }
}

/// Serializes tests that install a backend.
///
/// [`super::install_backend`] is process-global (like `failpoint::arm` and
/// `testsupport::cwd_guard`), so two tests installing phantoms concurrently would observe each
/// other's hardware. Take this before installing; hold it for the test body.
pub fn test_lock() -> &'static Mutex<()> {
    static LOCK: Mutex<()> = Mutex::new(());
    &LOCK
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::synth::{synthesize, SynthCtx};
    use crate::transport;

    /// The one getter `synthesize` qualifies a pipe with — a phantom that stays mute here is
    /// treated as "not a talking razer_report pipe" and synthesis returns `None`.
    const FIRMWARE: (u8, u8) = (0x00, 0x81);

    fn guarded<T>(body: impl FnOnce() -> T) -> T {
        let _serial = test_lock().lock().unwrap_or_else(PoisonError::into_inner);
        body()
    }

    // ── the seam itself ───────────────────────────────────────────────────────────────────────

    #[test]
    fn installed_backend_replaces_real_enumeration_and_restores_on_drop() {
        guarded(|| {
            let mut b = MockBackend::new();
            b.with(MockDevice::razer(0x00A8, "Phantom Naga"));
            b.with(MockDevice::razer(0x0221, "Phantom BlackWidow"));

            {
                let _g = transport::install_backend(Arc::new(b));
                let found = transport::enumerate().expect("phantom enumeration");
                assert_eq!(found.len(), 2, "enumeration sees exactly the phantoms");
                let pids: Vec<u16> = found.iter().map(|i| i.pid).collect();
                assert!(pids.contains(&0x00A8) && pids.contains(&0x0221));
                assert!(found.iter().all(|i| i.vid == 0x1532));
            }

            // The guard dropped, so the policy reverted to this build's default — which under
            // cfg(test) is Denied. The phantoms are gone AND we did not fall back to the wire.
            let after = transport::enumerate().expect("denied enumerate is an honest empty bus");
            assert!(
                after.is_empty(),
                "after the guard drops a test must see an EMPTY bus, not the maintainer's real \
                 devices — got {} device(s)",
                after.len()
            );
        });
    }

    /// An INNER guard must restore the OUTER guard's policy, not the build's default.
    ///
    /// `BackendGuard::drop` used to clear the policy to `None`, which resolves through
    /// `default_policy()`. Under `cfg(test)` that default is `Denied`, so this crate's own suite
    /// could never feel the difference — but a DOWNSTREAM crate enabling `mock-transport` compiles
    /// neuron-core WITHOUT `cfg(test)`, where the default is `Real`. There, dropping an inner
    /// phantom while an enclosing `deny_hardware()` guard was still alive silently handed the
    /// process back to THE REAL WIRE, defeating the whole point of the policy. Only a NESTED scope
    /// exposes it, which is why this test exists next to the single-guard one above.
    #[test]
    fn an_inner_guard_restores_the_outer_policy_not_the_default() {
        guarded(|| {
            let mut outer = MockBackend::new();
            outer.with(MockDevice::razer(0x00A8, "Outer Naga"));
            let mut inner = MockBackend::new();
            inner.with(MockDevice::razer(0x0221, "Inner BlackWidow"));
            inner.with(MockDevice::razer(0x0222, "Inner Second"));

            let _o = transport::install_backend(Arc::new(outer));
            {
                let _i = transport::install_backend(Arc::new(inner));
                assert_eq!(
                    transport::enumerate().expect("inner phantom enumeration").len(),
                    2,
                    "the inner phantom owns the bus while its guard is alive"
                );
            }

            // The inner guard is gone; the OUTER guard is still in scope and must still own the bus.
            let back = transport::enumerate().expect("outer phantom restored");
            assert_eq!(
                back.len(),
                1,
                "dropping the inner guard must restore the OUTER phantom, not fall through to the \
                 build default (an empty bus here under cfg(test), but the REAL WIRE for a \
                 downstream crate using mock-transport) — got {} device(s)",
                back.len()
            );
            assert_eq!(back[0].pid, 0x00A8, "and it must be the outer phantom, not the inner one");
        });
    }

    #[test]
    fn a_test_cannot_reach_real_hardware_without_asking_for_it() {
        guarded(|| {
            // No guard, no policy set: this is what every test in this crate gets by default.
            assert!(
                transport::enumerate()
                    .expect("enumerate succeeds")
                    .is_empty(),
                "the DEFAULT policy under cfg(test) must be Denied — otherwise a test that \
                 forgets to install a phantom silently drives the developer's own devices, which \
                 is how profile.rs came to write dpi=16000 to a live Naga"
            );
            // `Box<dyn Transport>` is not Debug, so match rather than `expect_err`.
            let real_looking = DevicePath::from_str_for_tests("\\\\?\\hid#vid_1532&pid_00a8");
            match transport::open_path(&real_looking) {
                Ok(_) => panic!(
                    "opening a device must FAIL under the denied policy, not reach the wire"
                ),
                Err(e) => assert!(
                    e.to_string().contains("denied"),
                    "the refusal must name the policy so the fix is obvious: {e}"
                ),
            }
        });
    }

    #[test]
    fn a_device_opens_over_a_phantom_and_every_request_is_logged() {
        guarded(|| {
            let mut b = MockBackend::new();
            let phantom = b.with(
                MockDevice::razer(0x00A8, "Phantom Naga")
                    .answering(FIRMWARE.0, FIRMWARE.1, &[0x01, 0x02]),
            );
            let _g = transport::install_backend(Arc::new(b));

            let t = transport::open_path(&phantom.info.path).expect("open phantom");
            let mut req = [0u8; BUF_LEN];
            req[7] = FIRMWARE.0;
            req[8] = FIRMWARE.1;
            t.set_feature(&req).expect("request lands");
            let mut reply = [0u8; BUF_LEN];
            t.get_feature(&mut reply).expect("reply arrives");

            assert_eq!(reply[1], 0x02, "status SUCCESS");
            assert_eq!((reply[7], reply[8]), FIRMWARE, "reply echoes the header");
            assert_eq!(&reply[9..11], &[0x01, 0x02], "body carries the scripted args");
            assert_eq!(phantom.asked(), vec![FIRMWARE], "the phantom logged the ask");
        });
    }

    // ── hostile firmware ──────────────────────────────────────────────────────────────────────

    #[test]
    fn a_silent_pipe_never_echoes_so_a_poll_loop_keeps_polling() {
        let phantom = Arc::new(
            MockDevice::razer(0x1234, "Mute")
                .answering(FIRMWARE.0, FIRMWARE.1, &[0xAB])
                .faulting(Fault::Silence),
        );
        let t = phantom.handle();
        let mut req = [0u8; BUF_LEN];
        req[7] = FIRMWARE.0;
        req[8] = FIRMWARE.1;
        t.set_feature(&req).expect("request lands even on a mute pipe");

        let mut reply = [0u8; BUF_LEN];
        t.get_feature(&mut reply).expect("read succeeds");
        assert_eq!(
            crate::protocol::reply_status(&reply, FIRMWARE.0, FIRMWARE.1),
            None,
            "a silent pipe must not look like an answer — the loop has to keep polling"
        );
    }

    #[test]
    fn a_yank_kills_this_call_and_every_later_one() {
        let phantom = Arc::new(
            MockDevice::razer(0x1234, "Yanked")
                .answering(FIRMWARE.0, FIRMWARE.1, &[0x01])
                .faulting(Fault::Yank),
        );
        let t = phantom.handle();
        let mut req = [0u8; BUF_LEN];
        req[7] = FIRMWARE.0;
        req[8] = FIRMWARE.1;
        assert!(t.set_feature(&req).is_err(), "the yank surfaces immediately");
        assert!(
            t.set_feature(&req).is_err(),
            "and the device stays gone — no phantom resurrection"
        );
        let mut reply = [0u8; BUF_LEN];
        assert!(t.get_feature(&mut reply).is_err(), "reads are gone too");
    }

    #[test]
    fn cross_talk_is_rejected_by_the_echo_filter() {
        let phantom = Arc::new(
            MockDevice::razer(0x1234, "Chatty")
                .answering(FIRMWARE.0, FIRMWARE.1, &[0x01])
                .faulting(Fault::CrossTalk),
        );
        let t = phantom.handle();
        let mut req = [0u8; BUF_LEN];
        req[7] = FIRMWARE.0;
        req[8] = FIRMWARE.1;
        t.set_feature(&req).unwrap();
        let mut reply = [0u8; BUF_LEN];
        t.get_feature(&mut reply).unwrap();
        assert_eq!(
            crate::protocol::reply_status(&reply, FIRMWARE.0, FIRMWARE.1),
            None,
            "another conversation's reply must not satisfy this one"
        );
    }

    #[test]
    fn two_handles_share_one_wire_lock() {
        let phantom = Arc::new(MockDevice::razer(0x1234, "Shared"));
        let a = phantom.handle();
        let b = phantom.handle();
        let (la, lb) = (a.wire_lock(), b.wire_lock());
        assert!(la.is_some() && lb.is_some());
        assert!(
            Arc::ptr_eq(&la.unwrap(), &lb.unwrap()),
            "two handles on one DevicePath must share the wire lock, or their request/reply \
             pairs interleave and cross-read"
        );
    }

    // ── executable evidence of two confirmed defects ──────────────────────────────────────────
    //
    // These assert the behaviour synthesis should have. Both defects are now FIXED and the tests
    // run in the normal suite — the acceptance criterion was exactly "delete the ignore".

    /// FIXED. Synthesis now opens with a CANARY: a command no device implements. A board that
    /// answers SUCCESS to that has invalidated every other answer it could give, so synthesis
    /// refuses the pipe outright instead of minting a capability map — writes included — from
    /// what is provably noise. Trusting the status byte was the whole defect; the canary is what
    /// makes the status byte trustworthy.
    #[test]
    fn a_parroting_board_must_not_mint_capabilities_it_never_proved() {
        let phantom = Arc::new(MockDevice::razer(0x9999, "Parrot").parroting());
        let ctx = SynthCtx::from_info(&phantom.info);
        assert!(
            synthesize(&phantom.handle(), &ctx).is_none(),
            "a pipe that rubber-stamps every command has proved nothing, so there is no honest \
             def to emit for it — any capability minted here would be entirely forged"
        );
    }

    /// FIXED. Lighting evidence is now per-COMMAND (`CatalogEntry::proves_lighting`) rather than
    /// per class, so answering the class-0x03 game-mode getter — a keyboard POLICY read about the
    /// Win-key kill — no longer implies class-0x03 LIGHTING. The old inference grew a full legacy
    /// lighting block, carrying unprobed 0x03/0x0A + 0x03/0x0B writes at tx 0x3F, onto boards with
    /// no addressable LEDs at all.
    #[test]
    fn a_board_with_only_game_mode_must_not_grow_a_lighting_block() {
        let phantom = Arc::new(
            MockDevice::razer(0x8888, "Game mode only")
                .answering(FIRMWARE.0, FIRMWARE.1, &[0x01, 0x00])
                // The Win-key-kill getter. It is a KEYBOARD POLICY read. It says nothing
                // whatsoever about whether this board has addressable lighting.
                .answering(0x03, 0x80, &[0x00, 0x08, 0x00]),
        );
        let ctx = SynthCtx::from_info(&phantom.info);
        let s = synthesize(&phantom.handle(), &ctx).expect("qualifies");

        assert!(
            s.def.lighting.is_none(),
            "no lighting getter answered, so no lighting block may be emitted — got {:?}",
            s.def.lighting.as_ref().map(|l| (l.protocol, l.rows, l.cols))
        );
    }
}
