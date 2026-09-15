// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

//! Platform-agnostic transport: send/receive HID feature reports to the control pipe.
//!
//! Windows uses the native `windows-sys` path (open with access=0, feature IOCTLs are
//! FILE_ANY_ACCESS — which is how you talk to a protected HID mouse). Other platforms
//! can drop in a hidapi/hidraw impl behind the same trait later.
//!
//! [`Transport::wire_lock`] serializes conversations from separate handles opened on the SAME
//! `DevicePath` to prevent cross-read replies — within this process via a local `Mutex`,
//! and ACROSS processes on Windows via a named kernel mutex layered underneath ([`WireLock`]):
//! the CLI and the app now serialize against each other's request/reply pairs too. The kernel
//! half is best-effort by design (bounded 2s wait, local-only degradation on create failure) so
//! a hung foreign process can never deadlock a user command.

use anyhow::Result;
use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::sync::{Arc, LazyLock, Mutex, PoisonError};
// Only the backend-policy cell needs this, and that cell only exists in test/mock builds.
#[cfg(any(test, feature = "mock-transport"))]
use std::sync::RwLock;

/// An opaque handle key identifying one enumerated HID interface.
///
/// The portable layer (this trait + `device.rs`) only stores, clones, and compares it — it never
/// inspects the contents. Each platform backend is the sole place that knows the encoding: on
/// Windows it's a UTF-16 device-interface path consumed by `CreateFileW`; a future hidraw/IOKit
/// backend keys on a `CString`/`&str`. `OsString` is the platform-native opaque string that lets
/// every backend round-trip its own native path losslessly.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct DevicePath(OsString);

impl DevicePath {
    /// The backend converts this to its native path type (e.g. wide chars on Windows).
    pub fn as_os_str(&self) -> &OsStr {
        &self.0
    }

    /// Build from a Windows wide string, stripping a trailing NUL if present so the stored key is
    /// the bare path. (The NUL is re-added on demand by [`to_wide_nul`].)
    #[cfg(windows)]
    pub fn from_wide(w: &[u16]) -> DevicePath {
        use std::os::windows::ffi::OsStringExt;
        let trimmed = match w.last() {
            Some(0) => &w[..w.len() - 1],
            _ => w,
        };
        DevicePath(OsString::from_wide(trimmed))
    }

    /// The NUL-terminated wide string `CreateFileW` consumes. The terminator is centralized here so
    /// no caller can forget it.
    #[cfg(windows)]
    pub fn to_wide_nul(&self) -> Vec<u16> {
        use std::os::windows::ffi::OsStrExt;
        self.0.encode_wide().chain(std::iter::once(0)).collect()
    }

    /// TEST SEAM — build a `DevicePath` from a plain string, cross-platform (the real
    /// constructors are Windows-only wide-string ceremony). Lets other crates' tests (e.g.
    /// neuron-host's `bridge::discover_from` tests) construct synthetic HID paths without
    /// depending on a platform backend. Not for production use — real paths come from
    /// `enumerate()`.
    #[doc(hidden)]
    pub fn from_str_for_tests(s: &str) -> DevicePath {
        DevicePath(OsString::from(s))
    }

    /// Build from a native path string — the Linux (and any future plain-`OsString`-keyed POSIX)
    /// backend's production constructor, the non-Windows counterpart to [`from_wide`](Self::from_wide).
    /// No wide-string ceremony needed off Windows: the platform's own path bytes round-trip
    /// losslessly through `OsString` already.
    #[cfg(not(windows))]
    pub fn from_str(s: &str) -> DevicePath {
        DevicePath(OsString::from(s))
    }
}

/// One enumerated HID collection.
pub struct HidDeviceInfo {
    pub vid: u16,
    pub pid: u16,
    pub usage_page: u16,
    pub usage: u16,
    pub feature_len: u16,
    /// The collection's OUTPUT/INPUT report byte lengths (HIDP_CAPS `OutputReportByteLength` /
    /// `InputReportByteLength`), the second wire surface's signature. `feature_len` above is the
    /// razer_report control pipe's shape; these are what a request/reply-over-output/input family
    /// (HID++: a 7-byte short or 20-byte long report) is recognized by. Zero when the OS reports no
    /// output/input report on this collection (feature-only pipes). Kept alongside `feature_len` so
    /// a `Dialect::claims` can test whichever surface it rides.
    pub input_len: u16,
    pub output_len: u16,
    pub path: DevicePath, // platform-opaque handle key
    /// The device's own USB product string (e.g. "Razer Naga V2 Pro"), empty when the
    /// OS/device doesn't offer one. Used to give auto-synthesized device defs an honest
    /// name instead of a bare pid.
    pub product: String,
}

impl HidDeviceInfo {
    /// The identity of the PHYSICAL unit this collection belongs to — see [`path_instance`].
    pub fn instance(&self) -> String {
        path_instance(&self.path.0.to_string_lossy())
    }
}

/// Reduce a raw HID device-interface path to the identity of the PHYSICAL device it belongs to —
/// the thing that tells "two collections of one device" apart from "two identical devices".
/// This is the app-wide per-UNIT identity: everything that must address one specific physical
/// unit (the device panel's rows, the selected-device control plane, the host bridge's surface
/// keys) derives it from here, so they can never disagree.
///
/// Heuristic over the Windows HID path shape, e.g.
/// `\\?\hid#vid_1532&pid_0221&mi_01&col02#8&2f5ca30f&0&0001#{4d1e55b2-f16f-11cf-88cb-001111000030}`:
/// - `mi_XX` (multiple-interface index) and `colXX` (collection index) are interface-level, not
///   device-level, so they're stripped — a keyboard's several collections (main + consumer
///   control + vendor) must collapse to ONE instance.
/// - the trailing `#{guid}` is the device-interface-CLASS guid (identical for every unit of the
///   same kind of HID device) — stripped too, it carries no per-unit information.
/// - what survives — vid/pid plus the container id (`8&2f5ca30f&0&0001`) — is what actually
///   differs between two identical devices plugged into different USB ports.
///
/// Linux `/sys/...` shape (see `transport/hidraw.rs`), e.g.
/// `/sys/devices/pci0000:00/usb1/1-2/1-2:1.0/0003:1532:0091.0001/hidraw/hidraw0#col01`:
/// the USB DEVICE node is the bare bus-port component (`1-2`, or `1-2.4` behind a hub) that
/// appears BEFORE its own interface's child node (`1-2:1.0` = device `1-2`, config 1, interface
/// 0). Truncating the path to end at that component — dropping the interface, the HID bus device
/// (which embeds a global per-interface counter, e.g. `...0091.0001`), the hidraw node name, and
/// the `#colNN` suffix — collapses every interface and collection of one physical mouse to one
/// instance, while two identical mice on different ports keep their different bus-port component
/// and stay distinct. A Windows-shaped path has no `/`-separated component matching this pattern,
/// so this is a no-op for it.
fn truncate_at_usb_device_node(path: &str) -> &str {
    fn is_usb_device_component(s: &str) -> bool {
        !s.is_empty()
            && s.contains('-')
            && !s.contains(':')
            && s.chars().all(|c| c.is_ascii_digit() || c == '-' || c == '.')
    }
    let parts: Vec<&str> = path.split('/').collect();
    match parts.iter().rposition(|s| is_usb_device_component(s)) {
        Some(idx) => {
            // Byte offset of the end of the matched component: sum of every component up to and
            // including it, plus its separating '/' — minus the one trailing separator that isn't
            // actually in the string.
            let end = parts[..=idx].iter().map(|s| s.len() + 1).sum::<usize>() - 1;
            &path[..end]
        }
        None => path,
    }
}

/// Regex-free by design (no new dependency): lowercase + segment filtering only.
pub fn path_instance(path: &str) -> String {
    let truncated = truncate_at_usb_device_node(path);
    let lower = truncated.to_ascii_lowercase();
    // Drop the trailing "#{...}" interface-class guid, if present.
    let without_guid = match lower.rfind("#{") {
        Some(i) => &lower[..i],
        None => lower.as_str(),
    };
    without_guid
        .split('#')
        .map(|segment| {
            segment
                .split('&')
                .filter(|part| !(part.starts_with("mi_") || part.starts_with("col")))
                .collect::<Vec<_>>()
                .join("&")
        })
        .collect::<Vec<_>>()
        .join("#")
}

/// Process-global DevicePath → wire-lock registry. Strong Arcs, never evicted: the set of
/// control pipes on one machine in one boot is tiny (a handful), and a stable Arc means two
/// opens at any two times always share the same lock.
static WIRE_LOCKS: LazyLock<Mutex<HashMap<DevicePath, Arc<WireLock>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Resolve (creating on first sight) the shared wire lock for `path`. Every backend's `open()`
/// calls this so two handles onto the same control pipe — the host lighting writer and a
/// runtime `apply_effect`, say — hold the IDENTICAL `Arc<WireLock>` and so serialize against
/// each other's request/reply conversations (see the module-level locking contract).
pub(crate) fn wire_lock_for(path: &DevicePath) -> Arc<WireLock> {
    let mut locks = WIRE_LOCKS.lock().unwrap_or_else(PoisonError::into_inner);
    locks
        .entry(path.clone())
        .or_insert_with(|| Arc::new(WireLock::for_path(path)))
        .clone()
}

/// The per-pipe wire lock: a process-local `Mutex` (fast path, poison-recovered) LAYERED over a
/// named kernel mutex on Windows, so pair-atomicity holds across PROCESSES too — the app's host
/// writer and a `neuron-cli` write no longer interleave SetFeature/GetFeature pairs, closing the
/// cross-process form of a race that was previously fixed in-process only.
///
/// Semantics, in acquisition order:
/// 1. the LOCAL mutex first (cheap, and it means at most one thread per process ever waits on the
///    kernel object — the OS wait below can never be contended by our own threads);
/// 2. then the KERNEL mutex, with a BOUNDED 2s wait. Timeout/failure degrades to proceeding with
///    only the local lock — never a deadlock behind a hung foreign process, and never worse than
///    the pre-cross-process behavior. (A conversation legitimately holds for ≤~600ms worst-case;
///    a 2s-held wire means the holder is wedged and the device is already toast.)
///    `WAIT_ABANDONED` counts as acquired: the previous holder DIED mid-conversation — ownership
///    transfers to us, and the reply-echo filter already tolerates whatever half-conversation the
///    corpse left on the pipe (same story as a same-process crash before this layer existed).
///
/// Construction is infallible: if the kernel object can't be created (name collision with a
/// foreign object type, exotic ACL environment), `os` is `None` and the lock is process-local —
/// graceful degradation, loudly documented rather than silently assumed away.
pub struct WireLock {
    local: Mutex<()>,
    #[cfg(windows)]
    os: Option<windows_hid::OsWireMutex>,
    /// The `flock(2)`-based kernel half on Linux — see `transport/hidraw.rs::OsWireFlock`. Same
    /// role as `os` above, just a different OS primitive.
    #[cfg(target_os = "linux")]
    os: Option<hidraw::OsWireFlock>,
}

impl WireLock {
    /// A process-local-only lock — for test fakes and platforms with no kernel half.
    pub fn new_local() -> WireLock {
        WireLock {
            local: Mutex::new(()),
            #[cfg(windows)]
            os: None,
            #[cfg(target_os = "linux")]
            os: None,
        }
    }

    /// The full lock for a real device pipe: local mutex + the OS kernel half (Windows: a named
    /// mutex; Linux: an `flock`) derived from the path, shared by every neuron process that opens
    /// this pipe.
    fn for_path(path: &DevicePath) -> WireLock {
        #[cfg(windows)]
        {
            WireLock {
                local: Mutex::new(()),
                os: windows_hid::OsWireMutex::for_path(path),
            }
        }
        #[cfg(target_os = "linux")]
        {
            WireLock {
                local: Mutex::new(()),
                os: hidraw::OsWireFlock::for_path(path),
            }
        }
        #[cfg(not(any(windows, target_os = "linux")))]
        {
            let _ = path;
            WireLock::new_local()
        }
    }

    /// Hold the wire for ONE request/reply conversation. See the type doc for the ordering and
    /// degradation rules. The guard releases both halves on drop (kernel half only if acquired).
    pub fn acquire(&self) -> WireGuard<'_> {
        let local = self.local.lock().unwrap_or_else(PoisonError::into_inner);
        #[cfg(windows)]
        let os_held = self.os.as_ref().is_some_and(|m| m.acquire());
        #[cfg(target_os = "linux")]
        let os_held = self.os.as_ref().is_some_and(|m| m.acquire());
        WireGuard {
            _local: local,
            #[cfg(windows)]
            os: if os_held { self.os.as_ref() } else { None },
            #[cfg(target_os = "linux")]
            os: if os_held { self.os.as_ref() } else { None },
        }
    }
}

/// RAII guard from [`WireLock::acquire`]. `!Send` by construction (holds a `MutexGuard`), which
/// also guarantees the kernel mutex is released by the thread that acquired it — a Win32
/// `ReleaseMutex` requirement (and, on Linux, keeps `flock`'s acquire/release on the one thread
/// that logically owns the conversation, even though `flock` itself has no such requirement).
pub struct WireGuard<'a> {
    _local: std::sync::MutexGuard<'a, ()>,
    #[cfg(windows)]
    os: Option<&'a windows_hid::OsWireMutex>,
    #[cfg(target_os = "linux")]
    os: Option<&'a hidraw::OsWireFlock>,
}

#[cfg(windows)]
impl Drop for WireGuard<'_> {
    fn drop(&mut self) {
        if let Some(m) = self.os {
            m.release();
        }
    }
}

#[cfg(target_os = "linux")]
impl Drop for WireGuard<'_> {
    fn drop(&mut self) {
        if let Some(m) = self.os {
            m.release();
        }
    }
}

/// A feature-report channel to one device.
///
/// Two wire surfaces live here. The PROVEN one is the feature-report request/reply pair
/// ([`set_feature`](Transport::set_feature)/[`get_feature`](Transport::get_feature)) — how
/// `razer_report` talks (a SetFeature IOCTL request, a GetFeature IOCTL reply). The SECOND surface
/// ([`write_output`](Transport::write_output)/[`read_input`](Transport::read_input)) is for
/// families whose requests ride an OUTPUT report (`WriteFile`) and whose replies arrive as INPUT
/// reports (`ReadFile`) — the shape HID++ uses (DIALECT-RND survey ruling: "HID++ requests ride
/// `WriteFile`(output report) and replies arrive as input reports", distinct from razer's feature
/// pull). Both default to an honest error so every existing impl — the Windows feature-report
/// transport, the synth/dialect test mocks — compiles unchanged and only a family that needs the
/// output/input surface overrides them.
pub trait Transport {
    fn set_feature(&self, buf: &[u8]) -> Result<()>;

    /// Pull a feature report into `buf`; returns HOW MANY BYTES were actually filled.
    ///
    /// The count is the whole point. This used to return `()`, which made a SHORT READ structurally
    /// invisible: a device that answers a few bytes and goes quiet left the caller's zeroed buffer
    /// mostly untouched, `Report::from_buf` zero-filled the tail, and a reply that never happened
    /// verified successfully against any all-zero expectation — i.e. the round-trip verify that
    /// gates neuron's most dangerous writes could report "the write landed" for a device that said
    /// nothing. `writes::verify_getter`'s own short-read guard was likewise unreachable, since the
    /// payload it inspects is a fixed `[u8; 80]`. With a real count, the dialects can (and do)
    /// refuse a structurally short frame instead of trusting its zero-padding.
    fn get_feature(&self, buf: &mut [u8]) -> Result<usize>;

    /// Send an OUTPUT report (the request half of the output/input wire surface). Default: an
    /// honest error — a feature-report-only transport does not carry output reports.
    fn write_output(&self, buf: &[u8]) -> Result<()> {
        let _ = buf;
        anyhow::bail!("transport does not carry output reports")
    }

    /// Read the next INPUT report (the reply half), waiting at most `timeout_ms`; returns the byte
    /// count written into `buf`. Default: an honest error — a feature-report-only transport does
    /// not carry input reports. A timeout must surface as an `Err`, not a zero-length `Ok`, so a
    /// probe draining replies can tell "nothing arrived in the window" from "an empty report".
    fn read_input(&self, buf: &mut [u8], timeout_ms: u32) -> Result<usize> {
        let _ = (buf, timeout_ms);
        anyhow::bail!("transport does not carry input reports")
    }

    /// The per-pipe WIRE LOCK shared by every transport opened on the same DevicePath, or None for
    /// a transport with no shared identity (test fakes). A razer_report conversation is a
    /// SetFeature→GetFeature(s) pair on one firmware control pipe; two handles interleaving pairs
    /// cross-read replies. The conversation OWNER (a Dialect's exec/exec_fast, a
    /// probe loop) holds this for the duration of ONE request/reply conversation — not per call,
    /// which couldn't keep the pair atomic. Cross-PROCESS too on Windows: [`WireLock`] layers a
    /// named kernel mutex under the process-local one, so a `neuron-cli` write serializes against
    /// the app's writer as well (bounded wait + graceful local-only degradation — see `WireLock`).
    fn wire_lock(&self) -> Option<Arc<WireLock>> {
        None
    }
}

/// A read channel for device-INITIATED input reports — the unsolicited reports a device pushes on
/// its own (e.g. a Razer mouse announcing "DPI is now X" when you press its onboard DPI button).
/// Feature reports are pull (request/response); these are push, so they need their own handle opened
/// with read access. `Send` so a listener thread can own it.
pub trait InputReader: Send {
    /// Wait (BOUNDED — implementations must never block forever) for the next input report.
    /// `Ok(Some(n))` = a report landed, `n` bytes written into `buf` (may be 0 for an empty report
    /// — still means "device is alive"). `Ok(None)` = the wait's internal timeout elapsed with
    /// nothing to read — NOT an error, just "try again"; a caller's read loop must treat this
    /// exactly like a zero-length report (loop back, check any stop flag, re-issue the read).
    /// `Err` = the device/handle is gone (unplugged, I/O error) — the caller should stop.
    fn read(&self, buf: &mut [u8]) -> Result<Option<usize>>;
}

/// How a caller's read loop should react to one [`InputReader::read`] outcome. Every listener
/// thread (hidwatch, macrokeys, the seiren R&D probe) needs the identical three-way branch —
/// factoring it here means a copy-pasted match arm can't quietly reintroduce the old
/// block-forever bug (treating a timeout as an error, or an error as "keep listening").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadStep {
    /// A report arrived (`usize` may be 0 for an empty report).
    Data(usize),
    /// No report within the read's internal timeout — keep listening, no error.
    Idle,
    /// The device/handle is gone — stop the loop.
    Gone,
}

/// Classify one `InputReader::read` result into the loop action a caller should take. See
/// [`ReadStep`] for the contract each variant implies.
pub fn classify_read(result: Result<Option<usize>>) -> ReadStep {
    match result {
        Ok(Some(n)) => ReadStep::Data(n),
        Ok(None) => ReadStep::Idle,
        Err(_) => ReadStep::Gone,
    }
}

// ── the backend hook ───────────────────────────────────────────────────────────────────────────
//
// `enumerate`/`open_path`/`open_reader` were free functions with hardcoded bodies, and they are
// the ONLY doors to hardware: 36 production call sites reach `enumerate` alone (device.rs's three
// resolvers, synth::adopt_filtered's whole auto-adoption pipeline, discover, profile::apply,
// hidwatch, macrokeys, runtime, glue, both mains, the CLI, the host bridge). With no indirection
// there was nothing to intercept, so NOTHING downstream of a device open could be tested — and
// two `#[test]`s (profile.rs's apply pair) enumerate real HID and issue real DPI/polling/lighting
// WRITES to whatever is plugged into the developer's desk during `cargo test`.
//
// One `Backend` consulted at the top of those three functions fixes all of it with zero changes to
// any call site. The trait is `Send + Sync` because it lives in a static; the `Box<dyn Transport>`
// it hands back is deliberately NOT `Send` (a `Device` is born on, and never leaves, the thread
// that writes it — see bridge.rs) and is constructed on the calling thread, so the bound stops at
// the factory.
//
// Release builds pay NOTHING: outside `cfg(test)` / the `mock-transport` feature the policy is a
// compile-time constant, so the branch folds away and the call inlines to exactly the platform
// body it had before.
//
// ── why a THREE-state policy and not `Option<Backend>` ────────────────────────────────────────
//
// An `Option` has one fatal default: "nothing installed" means "use real hardware". That is the
// right default for the app and exactly the wrong one for a test — and it is not hypothetical.
// `profile.rs`'s two apply tests reached real HID and wrote `dpi = 16000` with stages
// `[800, 16000]` to the maintainer's own Naga during `cargo test`, then recorded it into the
// deployed `feel-intent.toml` that gets reasserted on wake. Both tests' comments assert the
// opposite ("in a test/CI env no Razer device is present") — the assumption was simply never
// enforceable, because there was nothing to enforce it WITH.
//
// So the default is inverted where it matters: under `cfg(test)` the policy starts DENIED and a
// test must say what it wants. `enumerate` then honestly reports an empty bus — which is the very
// world those comments describe, so they become true by construction instead of by luck — and any
// attempt to actually OPEN a device fails loudly rather than silently reaching the wire.
//
// This is a whole-class fix. It is not possible to write a new neuron-core test that touches the
// user's hardware by forgetting a gate; you have to ask for hardware by name.

/// A source of HID devices. Implement this to stand in for real hardware.
///
/// The three methods mirror the three free functions below; see [`install_backend`] for the
/// lifetime rules and [`crate::transport::mock`] for the shipped fake.
pub trait Backend: Send + Sync {
    fn enumerate(&self) -> Result<Vec<HidDeviceInfo>>;
    fn open_path(&self, path: &DevicePath) -> Result<Box<dyn Transport>>;
    fn open_reader(&self, path: &DevicePath) -> Result<Box<dyn InputReader>>;
}

/// Where this process's devices come from.
#[cfg(any(test, feature = "mock-transport"))]
#[derive(Clone)]
enum Policy {
    /// The platform HID backend — the real wire.
    Real,
    /// No hardware is reachable. The default under `cfg(test)`.
    Denied,
    /// A phantom stands in for the bus.
    Fake(Arc<dyn Backend>),
}

/// `None` = "not yet decided", resolved to the build's default on first read. A `RwLock<Option<_>>`
/// rather than a `LazyLock` so a guard can put back whatever was here before it (including `None`).
#[cfg(any(test, feature = "mock-transport"))]
static POLICY: RwLock<Option<Policy>> = RwLock::new(None);

/// Denied under test, Real otherwise. The single line that makes the hazard structural.
#[cfg(any(test, feature = "mock-transport"))]
fn default_policy() -> Policy {
    if cfg!(test) {
        Policy::Denied
    } else {
        Policy::Real
    }
}

#[cfg(any(test, feature = "mock-transport"))]
fn policy() -> Policy {
    POLICY
        .read()
        .unwrap_or_else(PoisonError::into_inner)
        .clone()
        .unwrap_or_else(default_policy)
}

#[cfg(any(test, feature = "mock-transport"))]
fn set_policy(p: Policy) -> BackendGuard {
    let mut slot = POLICY.write().unwrap_or_else(PoisonError::into_inner);
    // Hand the OUTGOING policy to the guard so drop can put it back verbatim. Clearing to `None`
    // instead would fall through to `default_policy()`, which is only coincidentally right: under
    // `cfg(test)` the default is `Denied`, but a DOWNSTREAM crate using the `mock-transport`
    // feature compiles neuron-core without `cfg(test)`, so its default is `Real`. Dropping an
    // inner phantom guard would then revert to the REAL WIRE while an enclosing `deny_hardware()`
    // guard was still in scope — silently re-opening the hazard the policy exists to close.
    let previous = slot.clone();
    *slot = Some(p);
    BackendGuard(previous)
}

/// Restores whatever policy was in force before this guard was created (including "not yet
/// decided") when dropped, so nested and overlapping scopes unwind correctly.
///
/// RAII because a leaked policy would silently mislead every LATER test in the process — a fake
/// left installed yields a green suite that never touched hardware, and a `Real` left installed
/// re-opens the exact hazard this type exists to close. Mirrors `failpoint::Armed`.
#[cfg(any(test, feature = "mock-transport"))]
#[must_use = "the policy reverts as soon as this guard is dropped"]
pub struct BackendGuard(Option<Policy>);

#[cfg(any(test, feature = "mock-transport"))]
impl Drop for BackendGuard {
    fn drop(&mut self) {
        *POLICY.write().unwrap_or_else(PoisonError::into_inner) = self.0.take();
    }
}

/// Serve every device from `backend` until the guard drops.
///
/// PROCESS-GLOBAL, like `failpoint::arm` and `testsupport::cwd_guard` — tests that set a policy
/// must serialize against each other ([`mock::test_lock`]) or they will observe each other's bus.
#[cfg(any(test, feature = "mock-transport"))]
pub fn install_backend(backend: Arc<dyn Backend>) -> BackendGuard {
    set_policy(Policy::Fake(backend))
}

/// Opt IN to the real wire, for `#[ignore]`d probes that genuinely need hardware
/// (`device.rs::live_stream_strategy_probe`). Naming it at the call site is the point:
/// touching the user's devices from a test should be a deliberate, greppable act.
#[cfg(any(test, feature = "mock-transport"))]
pub fn allow_real_hardware() -> BackendGuard {
    set_policy(Policy::Real)
}

/// Cut this process off from hardware until the guard drops.
///
/// `cfg(test)` only covers the crate being tested, so neuron-core's own tests get [`Policy::Denied`]
/// for free but DOWNSTREAM crates (neuron-app, neuron-cli, integration tests) link neuron-core as a
/// plain dependency and would still reach the wire. They enable `mock-transport` and call this once
/// in their test setup to inherit the same guarantee.
#[cfg(any(test, feature = "mock-transport"))]
pub fn deny_hardware() -> BackendGuard {
    set_policy(Policy::Denied)
}

#[cfg(any(test, feature = "mock-transport"))]
const DENIED: &str = "real hardware is denied in this build — install a phantom with \
                      transport::install_backend(), or opt in with transport::allow_real_hardware()";

/// Enumerate every present HID interface.
pub fn enumerate() -> Result<Vec<HidDeviceInfo>> {
    #[cfg(any(test, feature = "mock-transport"))]
    match policy() {
        Policy::Fake(b) => return b.enumerate(),
        // An honest empty bus, not an error: "I looked and found nothing" is a state the
        // production code already handles on every machine with no Razer gear attached, so
        // denied tests exercise a REAL branch rather than a synthetic failure.
        Policy::Denied => return Ok(Vec::new()),
        Policy::Real => {}
    }
    platform_enumerate()
}

/// Open one enumerated interface for feature-report conversations.
pub fn open_path(path: &DevicePath) -> Result<Box<dyn Transport>> {
    #[cfg(any(test, feature = "mock-transport"))]
    match policy() {
        Policy::Fake(b) => return b.open_path(path),
        Policy::Denied => anyhow::bail!("{DENIED}"),
        Policy::Real => {}
    }
    platform_open_path(path)
}

/// Open a collection for READING its device-initiated input reports. Fails on OS-protected
/// collections (the mouse/keyboard top-level collections deny `GENERIC_READ`); succeeds on the
/// vendor collections where event reports actually ride.
pub fn open_reader(path: &DevicePath) -> Result<Box<dyn InputReader>> {
    #[cfg(any(test, feature = "mock-transport"))]
    match policy() {
        Policy::Fake(b) => return b.open_reader(path),
        Policy::Denied => anyhow::bail!("{DENIED}"),
        Policy::Real => {}
    }
    platform_open_reader(path)
}

#[cfg(windows)]
mod windows_hid;

#[cfg(target_os = "linux")]
mod hidraw;

// Compiled on every platform — see the module doc for why (its tests should run in Windows CI
// too, since the byte-level parsing logic has no OS dependency). Its only production caller is
// `hidraw.rs`, so a non-Linux build has nothing that calls `parse` outside `#[cfg(test)]` — expected,
// not a bug.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
mod hid_descriptor;

#[cfg(any(test, feature = "mock-transport"))]
pub mod mock;

#[cfg(windows)]
fn platform_enumerate() -> Result<Vec<HidDeviceInfo>> {
    windows_hid::enumerate()
}

#[cfg(windows)]
fn platform_open_path(path: &DevicePath) -> Result<Box<dyn Transport>> {
    Ok(Box::new(windows_hid::WinHid::open(path)?))
}

#[cfg(windows)]
fn platform_open_reader(path: &DevicePath) -> Result<Box<dyn InputReader>> {
    Ok(Box::new(windows_hid::WinHidReader::open(path)?))
}

#[cfg(target_os = "linux")]
fn platform_enumerate() -> Result<Vec<HidDeviceInfo>> {
    hidraw::enumerate()
}

#[cfg(target_os = "linux")]
fn platform_open_path(path: &DevicePath) -> Result<Box<dyn Transport>> {
    Ok(Box::new(hidraw::HidRaw::open(path)?))
}

#[cfg(target_os = "linux")]
fn platform_open_reader(path: &DevicePath) -> Result<Box<dyn InputReader>> {
    Ok(Box::new(hidraw::HidRawReader::open(path)?))
}

#[cfg(not(any(windows, target_os = "linux")))]
fn platform_enumerate() -> Result<Vec<HidDeviceInfo>> {
    anyhow::bail!("transport not implemented on this platform yet (hidapi/IOKit backend pending)")
}

#[cfg(not(any(windows, target_os = "linux")))]
fn platform_open_path(_path: &DevicePath) -> Result<Box<dyn Transport>> {
    anyhow::bail!("transport not implemented on this platform yet (hidapi/IOKit backend pending)")
}

#[cfg(not(any(windows, target_os = "linux")))]
fn platform_open_reader(_path: &DevicePath) -> Result<Box<dyn InputReader>> {
    anyhow::bail!("transport not implemented on this platform yet (hidapi/IOKit backend pending)")
}

#[cfg(test)]
mod tests {
    use super::*;

    // Compile-time pin of the `WireGuard` doc's load-bearing soundness claim (transport.rs:216-218):
    // "`!Send` by construction (holds a `MutexGuard`), which also guarantees the kernel mutex is
    // released by the thread that acquired it — a Win32 `ReleaseMutex` requirement." That guarantee
    // currently rests ENTIRELY on `MutexGuard` happening to be `!Send` and nobody adding a manual
    // `unsafe impl Send for WireGuard` later; this assertion turns a future violation into a build
    // break instead of a silent soundness hole. `WireGuard<'a>` carries a lifetime, and auto-trait-ness
    // doesn't depend on the lifetime parameter's value, only on the fields, so pinning the `'static`
    // instantiation covers every instantiation.
    static_assertions::assert_not_impl_any!(WireGuard<'static>: Send);

    /// A feature-report-only transport (like a razer_report mock): it implements the pull surface
    /// and inherits the DEFAULT output/input bodies. Pins that a family which never carries
    /// output/input reports still gets an honest error, not silence, from the second surface.
    struct FeatureOnly;
    impl Transport for FeatureOnly {
        fn set_feature(&self, _buf: &[u8]) -> Result<()> {
            Ok(())
        }
        fn get_feature(&self, buf: &mut [u8]) -> Result<usize> {
            Ok(buf.len())
        }
    }

    /// The cross-process wire guarantee, exercised through the exact object topology two
    /// PROCESSES have: two SEPARATELY-OPENED handles to one NAMED kernel mutex. A kernel object
    /// is resolved by name and doesn't care which address space the handle lives in — this is the
    /// same syscall path (`CreateMutexW` open-existing → `WaitForSingleObject` → `ReleaseMutex`)
    /// the app and the CLI take; only the process boundary differs, which the kernel abstracts.
    /// Pins: (a) a second handle BLOCKS while the first holds (measured, not assumed), (b) the
    /// release hands over within the bounded wait, (c) `WireGuard`-style release actually works
    /// across handles.
    #[cfg(windows)]
    #[test]
    fn named_kernel_mutex_excludes_across_separate_handles() {
        // Unique per test process so parallel/repeated runs never collide on a stale name.
        let name = format!("Local\\neuron-wire-selftest-{}", std::process::id());
        let a = windows_hid::OsWireMutex::open_named(&name).expect("create the named mutex");
        let b = windows_hid::OsWireMutex::open_named(&name).expect("open a second handle to it");
        assert!(a.acquire(), "first handle acquires immediately");

        // Exclusion is a HAPPENS-BEFORE property, so prove it with ordering, not with a stopwatch.
        // The old shape timed the waiter and demanded `waited >= 100ms` against a 150ms hold — but
        // the waiter's clock started only once the OS scheduled the thread, so a 60ms scheduling
        // delay under parallel test load shrank the measurement below the floor and failed a
        // perfectly correct mutex. Now: the waiter announces it is about to block, and the test
        // asserts it CANNOT finish while we hold, then MUST finish once we release.
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let (got_tx, got_rx) = std::sync::mpsc::channel();
        let waiter = std::thread::spawn(move || {
            // A named mutex is recursive PER THREAD, so the exclusion proof must come from a
            // different thread — which is also the honest analogue of a different process.
            ready_tx.send(()).expect("waiter announces itself");
            assert!(b.acquire(), "second handle acquires once the first releases");
            let _ = got_tx.send(());
            b.release();
        });
        ready_rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("waiter thread started");
        // Load can only make this window LONGER, never shorter, so a slow machine cannot turn a
        // correct mutex into a failure — it can only make the proof stronger.
        assert!(
            got_rx
                .recv_timeout(std::time::Duration::from_millis(250))
                .is_err(),
            "the second handle acquired while the first still held it — a no-op acquire would do \
             exactly this, and the cross-process wire guarantee would be fiction"
        );
        a.release();
        got_rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("release must hand the named mutex over to the waiting handle");
        waiter.join().expect("waiter thread clean");
    }

    /// CHILD HALF of `cross_process_wire_exclusion_is_real` below — inert on a normal test run
    /// (no env var → returns immediately, shows as a trivially-passing test). When the parent
    /// test re-invokes this test binary with `NEURON_WIRE_TEST_NAME` set, this process opens that
    /// named mutex, ACQUIRES it, announces the hold on stdout, keeps it for 400ms, releases, and
    /// exits — the foreign-process holder the parent proves exclusion against.
    #[cfg(windows)]
    #[test]
    fn wire_child_holds_the_named_mutex() {
        let Ok(name) = std::env::var("NEURON_WIRE_TEST_NAME") else {
            return; // normal run: not a child — nothing to do
        };
        use std::io::Write;
        let m = windows_hid::OsWireMutex::open_named(&name).expect("child opens the named mutex");
        assert!(m.acquire(), "child acquires");
        println!("WIRE-CHILD-HELD");
        std::io::stdout().flush().ok();
        std::thread::sleep(std::time::Duration::from_millis(400));
        m.release();
    }

    /// The cross-process wire guarantee exercised ACROSS A REAL PROCESS BOUNDARY (review
    /// observation closed): this test spawns the test binary itself as a child (env-gated helper
    /// above), waits until the child announces it HOLDS the named mutex, then proves from THIS
    /// process that (a) a short acquire attempt times out while the child holds — real blocking,
    /// not a no-op — and (b) a full-budget acquire succeeds once the child releases. Two distinct
    /// PIDs, one kernel object: the exact topology of the app and the CLI on a user's desk.
    #[cfg(windows)]
    #[test]
    fn cross_process_wire_exclusion_is_real() {
        use std::io::BufRead;
        let name = format!("Local\\neuron-wire-xproc-{}", std::process::id());
        // Our handle FIRST, so the object outlives any child-side timing.
        let mine = windows_hid::OsWireMutex::open_named(&name).expect("parent opens the mutex");
        let exe = std::env::current_exe().expect("test binary path");
        let mut child = std::process::Command::new(exe)
            .args([
                "--exact",
                "transport::tests::wire_child_holds_the_named_mutex",
                "--nocapture",
            ])
            .env("NEURON_WIRE_TEST_NAME", &name)
            .stdout(std::process::Stdio::piped())
            .spawn()
            .expect("spawn the child test process");
        // Wait for the child's explicit HOLD announcement (libtest chatter precedes it).
        let stdout = child.stdout.take().expect("piped stdout");
        let mut lines = std::io::BufReader::new(stdout).lines();
        let held = loop {
            match lines.next() {
                Some(Ok(l)) if l.contains("WIRE-CHILD-HELD") => break true,
                Some(Ok(_)) => continue,
                _ => break false,
            }
        };
        assert!(held, "the child process must announce it acquired the mutex");
        // (a) While the CHILD PROCESS holds: a short probe from THIS process must time out.
        assert!(
            !mine.acquire_for(100),
            "acquire must BLOCK while another PROCESS holds the named mutex — \
             if this succeeded, cross-process exclusion is fiction"
        );
        // (b) After the child's 400ms hold ends: the full-budget acquire succeeds.
        assert!(
            mine.acquire_for(WIRE_OS_WAIT_MS_FOR_TESTS),
            "acquire must succeed once the foreign holder releases"
        );
        mine.release();
        let status = child.wait().expect("child exits");
        assert!(status.success(), "the child test process must itself pass");
    }

    /// The production wait budget, mirrored for the cross-process test's success half (the
    /// constant lives in the windows backend; tests get it through this alias so the test reads
    /// as "the real budget", not a magic number).
    #[cfg(windows)]
    const WIRE_OS_WAIT_MS_FOR_TESTS: u32 = 2000;

    #[test]
    fn default_output_input_surface_errors_honestly() {
        let t = FeatureOnly;
        assert!(
            t.write_output(&[0u8; 8]).is_err(),
            "a feature-only transport must not silently accept an output report"
        );
        let mut buf = [0u8; 20];
        assert!(
            t.read_input(&mut buf, 100).is_err(),
            "a feature-only transport must ERROR (not Ok(0)) when asked for an input report"
        );
    }

    // ── path_instance ──────────────────────────────────────────────────────────────────────────

    #[test]
    fn windows_shaped_paths_are_unchanged_by_the_linux_truncation() {
        // Windows-shaped path, matching `path_instance`'s own doc-comment example — the Linux
        // addition (`truncate_at_usb_device_node`) must be a no-op here: still collapses two
        // collections of one keyboard, still tells apart two different container ids.
        let a = r"\\?\hid#vid_1532&pid_0221&mi_01&col02#8&2f5ca30f&0&0001#{4d1e55b2-f16f-11cf-88cb-001111000030}";
        let a2 = r"\\?\hid#vid_1532&pid_0221&mi_00&col01#8&2f5ca30f&0&0001#{4d1e55b2-f16f-11cf-88cb-001111000030}";
        let b = r"\\?\hid#vid_1532&pid_0221&mi_01&col02#8&9999aaaa&0&0002#{4d1e55b2-f16f-11cf-88cb-001111000030}";
        assert_eq!(
            path_instance(a),
            path_instance(a2),
            "two collections of one Windows device must still collapse"
        );
        assert_ne!(
            path_instance(a),
            path_instance(b),
            "two different container ids must still stay distinct"
        );
    }

    #[test]
    fn linux_interfaces_of_one_device_collapse_to_one_instance() {
        let iface0 = "/sys/devices/pci0000:00/0000:00:14.0/usb1/1-2/1-2:1.0/0003:1532:0091.0001/hidraw/hidraw0#col01";
        let iface1 = "/sys/devices/pci0000:00/0000:00:14.0/usb1/1-2/1-2:1.1/0003:1532:0092.0002/hidraw/hidraw1#col01";
        assert_eq!(
            path_instance(iface0),
            path_instance(iface1),
            "two interfaces of the SAME physical device (same bus-port `1-2`) must collapse"
        );
    }

    #[test]
    fn linux_collections_behind_one_interface_collapse_to_one_instance() {
        let col1 = "/sys/devices/pci0000:00/0000:00:14.0/usb1/1-2/1-2:1.0/0003:1532:0091.0001/hidraw/hidraw0#col01";
        let col2 = "/sys/devices/pci0000:00/0000:00:14.0/usb1/1-2/1-2:1.0/0003:1532:0091.0001/hidraw/hidraw0#col02";
        assert_eq!(
            path_instance(col1),
            path_instance(col2),
            "two top-level collections behind the SAME hidraw node must collapse"
        );
    }

    #[test]
    fn linux_devices_on_different_ports_stay_distinct() {
        let port2 = "/sys/devices/pci0000:00/0000:00:14.0/usb1/1-2/1-2:1.0/0003:1532:0091.0001/hidraw/hidraw0#col01";
        let port3 = "/sys/devices/pci0000:00/0000:00:14.0/usb1/1-3/1-3:1.0/0003:1532:0093.0001/hidraw/hidraw2#col01";
        assert_ne!(
            path_instance(port2),
            path_instance(port3),
            "two identical mice on different ports (`1-2` vs `1-3`) must stay distinct"
        );
    }

    #[test]
    fn classify_read_maps_every_outcome() {
        assert_eq!(classify_read(Ok(Some(12))), ReadStep::Data(12));
        assert_eq!(classify_read(Ok(Some(0))), ReadStep::Data(0));
        assert_eq!(classify_read(Ok(None)), ReadStep::Idle);
        assert_eq!(
            classify_read(Err(anyhow::anyhow!("device gone"))),
            ReadStep::Gone
        );
    }

    /// A scripted fake `InputReader` — no OS handle, just a canned outcome per call — standing in
    /// for `WinHidReader` so the CALLER LOOP contract (the thing hidwatch/macrokeys/seiren_probe
    /// all copy) is provable headless: a timeout must never look like "device gone", and a real
    /// error must always end the loop.
    struct ScriptedReader {
        script: Mutex<std::vec::IntoIter<Result<Option<usize>>>>,
    }
    impl ScriptedReader {
        fn new(script: Vec<Result<Option<usize>>>) -> Self {
            ScriptedReader {
                script: Mutex::new(script.into_iter()),
            }
        }
    }
    impl InputReader for ScriptedReader {
        fn read(&self, _buf: &mut [u8]) -> Result<Option<usize>> {
            self.script
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .next()
                .expect("script exhausted — loop read past its scripted outcomes")
        }
    }

    /// Runs the exact match shape hidwatch.rs / macrokeys.rs use, against a boxed trait object —
    /// proving the loop only ever sees the trait's contract, never a concrete backend.
    fn run_loop_via_trait(reader: &dyn InputReader, iterations: usize) -> (u32, u32, bool) {
        let (mut data_hits, mut idle_hits, mut stopped) = (0u32, 0u32, false);
        let mut buf = [0u8; 8];
        for _ in 0..iterations {
            match classify_read(reader.read(&mut buf)) {
                ReadStep::Data(_) => data_hits += 1,
                ReadStep::Idle => idle_hits += 1,
                ReadStep::Gone => {
                    stopped = true;
                    break;
                }
            }
        }
        (data_hits, idle_hits, stopped)
    }

    #[test]
    fn timeout_keeps_the_loop_alive_and_error_stops_it() {
        // Two timeouts (device idle, no data) must NOT be mistaken for "gone" — the loop keeps
        // spinning through them and only stops on the real error.
        let reader = ScriptedReader::new(vec![
            Ok(None),
            Ok(None),
            Ok(Some(3)),
            Ok(None),
            Err(anyhow::anyhow!("unplugged")),
        ]);
        let (data_hits, idle_hits, stopped) = run_loop_via_trait(&reader, 10);
        assert_eq!(data_hits, 1, "the single real report must be counted");
        assert_eq!(idle_hits, 3, "every timeout must be treated as idle, not an error");
        assert!(stopped, "the Err outcome must terminate the loop");
    }
}
