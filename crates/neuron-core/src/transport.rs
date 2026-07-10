//! Platform-agnostic transport: send/receive HID feature reports to the control pipe.
//!
//! Windows uses the native `windows-sys` path (open with access=0, feature IOCTLs are
//! FILE_ANY_ACCESS — which is how you talk to a protected HID mouse). Other platforms
//! can drop in a hidapi/hidraw impl behind the same trait later.
//!
//! [`Transport::wire_lock`] serializes conversations from separate handles opened on the SAME
//! `DevicePath` (LIGHTING-MAP §5's cross-read bug) — within this process via a local `Mutex`,
//! and ACROSS processes on Windows via a named kernel mutex layered underneath ([`WireLock`]):
//! the CLI and the app now serialize against each other's request/reply pairs too. The kernel
//! half is best-effort by design (bounded 2s wait, local-only degradation on create failure) so
//! a hung foreign process can never deadlock a user command.

use anyhow::Result;
use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::sync::{Arc, LazyLock, Mutex, PoisonError};

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
/// Regex-free by design (no new dependency): lowercase + segment filtering only.
pub fn path_instance(path: &str) -> String {
    let lower = path.to_ascii_lowercase();
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
/// each other's request/reply conversations (see the module doc and LIGHTING-MAP §5).
pub(crate) fn wire_lock_for(path: &DevicePath) -> Arc<WireLock> {
    let mut locks = WIRE_LOCKS.lock().unwrap_or_else(PoisonError::into_inner);
    locks
        .entry(path.clone())
        .or_insert_with(|| Arc::new(WireLock::for_path(path)))
        .clone()
}

/// The per-pipe wire lock: a process-local `Mutex` (fast path, poison-recovered) LAYERED over a
/// named kernel mutex on Windows, so pair-atomicity holds across PROCESSES too — the app's host
/// writer and a `neuron-cli` write no longer interleave SetFeature/GetFeature pairs (the
/// LIGHTING-MAP §5 race, previously fixed in-process only).
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
}

impl WireLock {
    /// A process-local-only lock — for test fakes and non-Windows backends (no kernel half).
    pub fn new_local() -> WireLock {
        WireLock {
            local: Mutex::new(()),
            #[cfg(windows)]
            os: None,
        }
    }

    /// The full lock for a real device pipe: local mutex + (Windows) the named kernel mutex
    /// derived from the path, shared by every neuron process that opens this pipe.
    fn for_path(path: &DevicePath) -> WireLock {
        #[cfg(windows)]
        {
            WireLock {
                local: Mutex::new(()),
                os: windows_hid::OsWireMutex::for_path(path),
            }
        }
        #[cfg(not(windows))]
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
        WireGuard {
            _local: local,
            #[cfg(windows)]
            os: if os_held { self.os.as_ref() } else { None },
        }
    }
}

/// RAII guard from [`WireLock::acquire`]. `!Send` by construction (holds a `MutexGuard`), which
/// also guarantees the kernel mutex is released by the thread that acquired it — a Win32
/// `ReleaseMutex` requirement.
pub struct WireGuard<'a> {
    _local: std::sync::MutexGuard<'a, ()>,
    #[cfg(windows)]
    os: Option<&'a windows_hid::OsWireMutex>,
}

#[cfg(windows)]
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
    fn get_feature(&self, buf: &mut [u8]) -> Result<()>;

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
    /// cross-read replies (LIGHTING-MAP §5). The conversation OWNER (a Dialect's exec/exec_fast, a
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
/// with read access. `read` blocks until one report arrives (or the handle is closed). `Send` so a
/// listener thread can own it.
pub trait InputReader: Send {
    /// Block for the next input report; returns the number of bytes written into `buf`.
    fn read(&self, buf: &mut [u8]) -> Result<usize>;
}

#[cfg(windows)]
mod windows_hid;

#[cfg(windows)]
pub fn enumerate() -> Result<Vec<HidDeviceInfo>> {
    windows_hid::enumerate()
}

#[cfg(windows)]
pub fn open_path(path: &DevicePath) -> Result<Box<dyn Transport>> {
    Ok(Box::new(windows_hid::WinHid::open(path)?))
}

/// Open a collection for READING its device-initiated input reports. Fails on OS-protected
/// collections (the mouse/keyboard top-level collections deny `GENERIC_READ`); succeeds on the
/// vendor collections where event reports actually ride.
#[cfg(windows)]
pub fn open_reader(path: &DevicePath) -> Result<Box<dyn InputReader>> {
    Ok(Box::new(windows_hid::WinHidReader::open(path)?))
}

#[cfg(not(windows))]
pub fn enumerate() -> Result<Vec<HidDeviceInfo>> {
    anyhow::bail!("transport not implemented on this platform yet (hidapi backend pending)")
}

#[cfg(not(windows))]
pub fn open_path(_path: &DevicePath) -> Result<Box<dyn Transport>> {
    anyhow::bail!("transport not implemented on this platform yet (hidapi backend pending)")
}

#[cfg(not(windows))]
pub fn open_reader(_path: &DevicePath) -> Result<Box<dyn InputReader>> {
    anyhow::bail!("transport not implemented on this platform yet (hidapi backend pending)")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A feature-report-only transport (like a razer_report mock): it implements the pull surface
    /// and inherits the DEFAULT output/input bodies. Pins that a family which never carries
    /// output/input reports still gets an honest error, not silence, from the second surface.
    struct FeatureOnly;
    impl Transport for FeatureOnly {
        fn set_feature(&self, _buf: &[u8]) -> Result<()> {
            Ok(())
        }
        fn get_feature(&self, _buf: &mut [u8]) -> Result<()> {
            Ok(())
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
        let waiter = std::thread::spawn(move || {
            // A named mutex is recursive PER THREAD, so the exclusion proof must come from a
            // different thread — which is also the honest analogue of a different process.
            let t0 = std::time::Instant::now();
            assert!(b.acquire(), "second handle acquires once the first releases");
            b.release();
            t0.elapsed()
        });
        std::thread::sleep(std::time::Duration::from_millis(150));
        a.release();
        let waited = waiter.join().expect("waiter thread clean");
        assert!(
            waited >= std::time::Duration::from_millis(100),
            "the second handle provably BLOCKED on the first's hold (waited {waited:?}) — \
             a no-op acquire would return instantly and the wire guarantee would be fiction"
        );
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
}
