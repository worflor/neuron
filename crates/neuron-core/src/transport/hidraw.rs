// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

//! Linux HID transport via `/dev/hidrawN` (`libc::ioctl`/`write`/`poll`/`read`), enumerated from
//! `/sys/class/hidraw/`.
//!
//! ## Windows exposes one path per collection; Linux exposes one node per interface
//!
//! `HidD_GetCaps` on Windows already answers "how big is this collection's feature/input/output
//! report" per top-level collection, because Windows itself enumerates one HID path per top-level
//! collection. Linux only hands out one `/dev/hidrawN` per HID INTERFACE, which can carry several
//! top-level collections and several report IDs behind that one node — so this backend parses the
//! interface's report descriptor itself ([`super::hid_descriptor`]) and emits one [`HidDeviceInfo`]
//! per top-level Application collection, with a `#colNN` suffix on the [`DevicePath`] to keep them
//! addressable as distinct "devices" the way the rest of neuron already expects (see
//! `transport.rs`'s module doc and `HidDeviceInfo`).
//!
//! ## ioctl numbers
//!
//! `HIDIOCSFEATURE`/`HIDIOCGFEATURE` are defined in `include/uapi/linux/hidraw.h` as
//! `_IOC(_IOC_WRITE|_IOC_READ, 'H', 0x06/0x07, len)` — a VARIABLE-size ioctl whose encoded size
//! field the hidraw driver reads back out of the command number to know how many bytes to copy, so
//! it must be computed per call from the buffer length, not hardcoded. The generic `_IOC` encoding
//! (`include/uapi/asm-generic/ioctl.h`: 2-bit dir | 14-bit size | 8-bit type | 8-bit nr, in that
//! bit order from the top) is what x86_64 and aarch64 both use; a handful of exotic architectures
//! (mips, sparc, powerpc, alpha) define their OWN dir/size bit layout, so this file intentionally
//! fails to compile there (see the `compile_error!` below) rather than silently deriving a wrong
//! ioctl number for them. Both files were read from this machine's WSL2 install
//! (`/usr/include/linux/hidraw.h`, `/usr/include/asm-generic/ioctl.h`) while writing this, not
//! recalled from memory.
//!
//! The comment right above `HIDIOCSFEATURE`/`HIDIOCGFEATURE` in `hidraw.h` — "The first byte of
//! SFEATURE and GFEATURE is the report number" — is the Linux side of the same report-ID-first
//! convention `windows_hid.rs` documents for `HidD_Get/SetFeature`, so `Transport::set_feature`/
//! `get_feature`'s buffer contract is identical on both backends: byte 0 is the report number, 0
//! when the collection doesn't use report IDs.

use super::hid_descriptor;
use super::{wire_lock_for, DevicePath, HidDeviceInfo, InputReader, Transport, WireLock};
use anyhow::{bail, Result};
use std::ffi::CString;
use std::os::unix::io::RawFd;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
compile_error!(
    "neuron's hidraw backend only derives ioctl numbers for the generic asm-generic/ioctl.h \
     encoding (x86_64, aarch64). Other architectures (mips/sparc/powerpc/alpha/...) define a \
     different dir/size bit layout and would silently get the WRONG ioctl number here rather \
     than a compile error — add and verify that architecture's encoding before enabling it."
);

// include/uapi/linux/hidraw.h
const HIDIOC_MAGIC: u32 = b'H' as u32; // 0x48
const HIDIOCSFEATURE_NR: u32 = 0x06;
const HIDIOCGFEATURE_NR: u32 = 0x07;

// include/uapi/asm-generic/ioctl.h: bit layout shared by x86_64 and aarch64 (see the module doc
// and the `compile_error!` above).
const IOC_WRITE: u32 = 1;
const IOC_READ: u32 = 2;
const IOC_NRSHIFT: u32 = 0;
const IOC_TYPESHIFT: u32 = 8;
const IOC_SIZESHIFT: u32 = 16;
const IOC_DIRSHIFT: u32 = 30;
const IOC_SIZEMASK: u32 = 0x3FFF; // 14 bits

/// Build the `_IOC(_IOC_WRITE|_IOC_READ, 'H', nr, len)` request number for HIDIOCSFEATURE/
/// HIDIOCGFEATURE. `len` is masked to the 14-bit size field rather than asserted, matching the
/// kernel macro (`_IOC_TYPECHECK` doesn't apply here since the size is a runtime buffer length,
/// not a fixed C type) — a `len` over 16383 bytes silently truncates the size field, which no real
/// HID report ever approaches.
fn feature_ioctl(nr: u32, len: usize) -> libc::c_ulong {
    let size = (len as u32) & IOC_SIZEMASK;
    let dir = IOC_WRITE | IOC_READ;
    ((dir << IOC_DIRSHIFT) | (size << IOC_SIZESHIFT) | (HIDIOC_MAGIC << IOC_TYPESHIFT) | (nr << IOC_NRSHIFT))
        as libc::c_ulong
}

fn last_os_error() -> std::io::Error {
    std::io::Error::last_os_error()
}

/// `errno` for a failed syscall as an owned code, so callers can classify it (see [`is_disconnect`]
/// and [`device_err`]) without a second `errno()` read racing a subsequent call.
fn errno_of(err: &std::io::Error) -> i32 {
    err.raw_os_error().unwrap_or(0)
}

/// True for the errno values that mean "the device is gone": ENODEV ("Device was removed") and
/// ESHUTDOWN ("disabled ... such as a physical disconnect"), per the kernel's USB error codes.
/// Both get a distinctly-worded error so a caller's disconnect handling doesn't have to
/// string-match a generic I/O failure.
///
/// EPIPE is deliberately not here. It is the kernel's "Endpoint stalled": the device answered the
/// control transfer by refusing it, which is exactly what a device does for a report it does not
/// support. The same doc notes a host controller may also emit EPIPE in the window before the hub
/// driver processes a real removal, so presence is decided by re-enumerating, not by one errno.
fn is_disconnect(errno: i32) -> bool {
    errno == libc::ENODEV || errno == libc::ESHUTDOWN
}

/// The error for one failed device request, naming a stall as a stall rather than a disconnect.
fn device_err(what: &str, err: &std::io::Error) -> anyhow::Error {
    let errno = errno_of(err);
    if is_disconnect(errno) {
        anyhow::anyhow!("device disconnected: {err}")
    } else if errno == libc::EPIPE {
        anyhow::anyhow!("{what}: the device refused the request (endpoint stalled): {err}")
    } else {
        anyhow::anyhow!("{what} failed: {err}")
    }
}

/// Run a syscall, retrying while it fails with `EINTR`; returns its non-negative result or the
/// failing error. The feature ioctls and an output write block for the length of a USB control
/// transfer, so a signal delivered in that window (a timer, SIGCHLD, a terminal resize) would
/// otherwise surface as a failed device write for a request the device would have serviced.
/// Unlike [`poll_read`], these carry no deadline, so retrying cannot extend a caller's timeout.
fn retry_eintr(mut call: impl FnMut() -> isize) -> std::result::Result<isize, std::io::Error> {
    loop {
        let ret = call();
        if ret >= 0 {
            return Ok(ret);
        }
        let err = last_os_error();
        if errno_of(&err) != libc::EINTR {
            return Err(err);
        }
    }
}

// ── sysfs enumeration ──────────────────────────────────────────────────────────────────────────

/// Parse a `hidraw*/device/uevent` file's `HID_ID=bus:vendor:product` line (hex, each field 4-8
/// digits wide — real devices vary, so this parses whatever width is present rather than assuming
/// one). Pure and platform-independent so it's testable without touching `/sys`.
fn parse_hid_id(uevent: &str) -> Option<(u16, u16, u16)> {
    for line in uevent.lines() {
        if let Some(rest) = line.strip_prefix("HID_ID=") {
            let mut parts = rest.trim().split(':');
            let bus = u16::from_str_radix(parts.next()?, 16).ok()?;
            let vid = u16::from_str_radix(parts.next()?, 16).ok()?;
            let pid = u16::from_str_radix(parts.next()?, 16).ok()?;
            return Some((bus, vid, pid));
        }
    }
    None
}

const BUS_USB: u16 = 0x0003;
const BUS_BLUETOOTH: u16 = 0x0005;

/// FNV-1a over raw bytes — used both for the wire-lock file name (below) and available here for
/// anything else that needs a short, stable, cross-process-reproducible tag from a path. Not
/// `DefaultHasher`: its SipHash keys are randomized per process, so two processes (the CLI and the
/// app) would derive two different names for the identical device and never share a lock.
fn stable_hash(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        h = (h ^ b as u64).wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// Encode a `DevicePath` for one top-level collection: the hidraw node's canonical sysfs path,
/// plus a 1-based 2-digit collection index. `col_index` must be 1-99.
fn encode_path(canonical_sysfs_path: &str, col_index: u32) -> DevicePath {
    DevicePath::from_str(&format!("{canonical_sysfs_path}#col{col_index:02}"))
}

/// Split an encoded `DevicePath` back into its sysfs path and collection index. Pure (no `/sys`
/// access), so it's testable without real hardware; [`hidraw_dev_node`] is the part that touches
/// the path further (deriving `/dev/hidrawN`).
fn decode_path(raw: &str) -> Option<(&str, u32)> {
    let idx = raw.rfind("#col")?;
    let digits = raw.get(idx + 4..)?;
    if digits.len() != 2 || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some((&raw[..idx], digits.parse().ok()?))
}

/// The `/dev/hidrawN` node a `DevicePath` opens to — the basename of the sysfs path component
/// before `#colNN` (the canonical sysfs path ends in `.../hidraw/hidrawN`).
fn hidraw_dev_node(path: &DevicePath) -> Result<String> {
    let raw = path.as_os_str().to_string_lossy();
    let (sysfs_path, _col) = decode_path(&raw)
        .ok_or_else(|| anyhow::anyhow!("not a hidraw DevicePath: {raw:?}"))?;
    let basename = sysfs_path
        .rsplit('/')
        .next()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow::anyhow!("malformed hidraw sysfs path: {sysfs_path:?}"))?;
    Ok(format!("/dev/{basename}"))
}

/// Walk upward from the hidraw node's canonical sysfs path looking for the nearest ancestor with a
/// readable `product` attribute — the USB device node's (`manufacturer`/`product`/`serial` live
/// only on a `usb_device`, never on a `usb_interface` or a `hid` bus device, so the first hit while
/// walking up IS the physical USB device's own string, whatever the exact nesting depth). Bounded
/// so a Bluetooth device (no such ancestor) or an unexpected layout can't walk indefinitely.
fn usb_product_string(canonical_hidraw_path: &Path) -> Option<String> {
    let mut dir = canonical_hidraw_path.to_path_buf();
    for _ in 0..8 {
        dir = dir.parent()?.to_path_buf();
        if let Ok(s) = std::fs::read_to_string(dir.join("product")) {
            let s = s.trim();
            if !s.is_empty() {
                return Some(s.to_string());
            }
        }
    }
    None
}

/// Enumerate every present `/dev/hidrawN` interface, emitting one [`HidDeviceInfo`] per top-level
/// Application collection its report descriptor declares. `/sys/class/hidraw/*/device/uevent` and
/// `report_descriptor` are both world-readable, so this needs no `/dev` permissions — only opening
/// a collection for I/O does. No vendor filter: every USB (bus 0x0003) and Bluetooth (bus 0x0005)
/// HID interface is included, mirroring the Windows backend's unfiltered `enumerate`. A node this
/// process can't fully read (races with unplug, a permission-locked file) is silently skipped,
/// same as Windows' `query` returning `None`.
pub fn enumerate() -> Result<Vec<HidDeviceInfo>> {
    enumerate_in(Path::new("/sys/class/hidraw"))
}

/// [`enumerate`] against an arbitrary hidraw class root, so the walk can be driven over a synthetic
/// sysfs tree in tests. Nothing here opens `/dev`; it is pure filesystem reads plus descriptor
/// parsing.
fn enumerate_in(class_root: &Path) -> Result<Vec<HidDeviceInfo>> {
    let mut out = Vec::new();
    let entries = match std::fs::read_dir(class_root) {
        Ok(d) => d,
        // No hidraw class at all (module not loaded, or a non-Linux-HID system) — an honest empty
        // bus, matching how the Windows backend reports "nothing found" rather than erroring.
        Err(_) => return Ok(out),
    };
    for entry in entries.flatten() {
        let device_dir = entry.path().join("device");
        let Ok(uevent) = std::fs::read_to_string(device_dir.join("uevent")) else {
            continue;
        };
        let Some((bus, vid, pid)) = parse_hid_id(&uevent) else {
            continue;
        };
        if bus != BUS_USB && bus != BUS_BLUETOOTH {
            continue;
        }
        let Ok(desc_bytes) = std::fs::read(device_dir.join("report_descriptor")) else {
            continue;
        };
        let Ok(canonical) = std::fs::canonicalize(entry.path()) else {
            continue;
        };
        let canonical_str = canonical.to_string_lossy().into_owned();
        let product = usb_product_string(&canonical).unwrap_or_default();

        for (i, caps) in hid_descriptor::parse(&desc_bytes).into_iter().enumerate() {
            out.push(HidDeviceInfo {
                vid,
                pid,
                usage_page: caps.usage_page,
                usage: caps.usage,
                feature_len: caps.feature_len,
                input_len: caps.input_len,
                output_len: caps.output_len,
                path: encode_path(&canonical_str, (i + 1) as u32),
                product: product.clone(),
            });
        }
    }
    Ok(out)
}

// ── poll-bounded read, shared by the Transport's read_input and InputReader ──────────────────────

/// One bounded read: `poll(2)` up to `timeout_ms`, then `read(2)`. `Ok(Some(n))` = data, `Ok(None)`
/// = the wait elapsed with nothing to read (NOT an error), `Err` = a real failure — `ENODEV`/
/// `EPIPE` (device gone) get an explicit message, anything else the raw OS error. Retries both
/// `poll` and `read` on `EINTR`, so a caller never sees a signal-interrupted wait as either a
/// timeout or a failure — a signal must SHORTEN the remaining wait, never restart it, or a request/
/// reply caller's timeout (its actual answer, not a spurious wakeup — see `read_input`) could be
/// extended arbitrarily by repeated interruptions.
fn poll_read(fd: RawFd, buf: &mut [u8], timeout_ms: i32) -> Result<Option<usize>> {
    let mut pfd = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    let deadline = Instant::now() + Duration::from_millis(timeout_ms.max(0) as u64);
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let remaining_ms = remaining.as_millis().min(i32::MAX as u128) as i32;
        if remaining_ms == 0 && Instant::now() >= deadline {
            return Ok(None); // deadline exhausted, possibly across several EINTR retries
        }
        let ret = unsafe { libc::poll(&mut pfd, 1, remaining_ms) };
        if ret < 0 {
            let err = last_os_error();
            if errno_of(&err) == libc::EINTR {
                continue; // re-poll with whatever time is LEFT, not the original budget
            }
            bail!("poll(2) failed: {err}");
        }
        if ret == 0 {
            return Ok(None); // timed out, nothing to read
        }
        break;
    }
    loop {
        let n = unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) };
        if n < 0 {
            let err = last_os_error();
            let errno = errno_of(&err);
            if errno == libc::EINTR {
                continue;
            }
            if is_disconnect(errno) {
                bail!("device disconnected: {err}");
            }
            bail!("read(2) failed: {err}");
        }
        return Ok(Some(n as usize));
    }
}

fn open_rdwr(dev_node: &str) -> Result<RawFd> {
    let cpath = CString::new(dev_node).map_err(|_| anyhow::anyhow!("device node path has an embedded NUL"))?;
    let fd = unsafe { libc::open(cpath.as_ptr(), libc::O_RDWR | libc::O_CLOEXEC) };
    if fd < 0 {
        bail!("open {dev_node} failed: {}", last_os_error());
    }
    Ok(fd)
}

/// A control channel to one top-level collection's `/dev/hidrawN` interface. Feature reports
/// (`set_feature`/`get_feature`) and output reports (`write_output`) all ride this ONE fd, opened
/// `O_RDWR | O_CLOEXEC` — unlike Windows, hidraw has no separate "protected collection" access
/// tier to work around, so there's no fallback-to-access-0 story here.
pub struct HidRaw {
    fd: RawFd,
    /// Several top-level collections share ONE `/dev/hidrawN` node (see the module doc), so two
    /// `HidRaw`s on different collections of the SAME interface each open their OWN fd rather than
    /// sharing one — Linux offers no per-collection handle, so per-collection isolation the way
    /// Windows gets it for free doesn't exist here; two collections' conversations can still
    /// interleave on the wire. `wire_lock_for` keys on the FULL `DevicePath` (sysfs path + colNN),
    /// so this only serializes handles opened on the SAME collection, same as Windows — a known,
    /// documented gap versus Windows' natural per-collection isolation, not a regression this
    /// backend can close without a kernel-side interface split.
    wire: Arc<WireLock>,
}

impl HidRaw {
    pub fn open(path: &DevicePath) -> Result<Self> {
        let dev_node = hidraw_dev_node(path)?;
        let fd = open_rdwr(&dev_node)?;
        Ok(HidRaw {
            fd,
            wire: wire_lock_for(path),
        })
    }
}

impl Drop for HidRaw {
    fn drop(&mut self) {
        unsafe {
            libc::close(self.fd);
        }
    }
}

impl Transport for HidRaw {
    fn wire_lock(&self) -> Option<Arc<WireLock>> {
        Some(self.wire.clone())
    }

    fn set_feature(&self, buf: &[u8]) -> Result<()> {
        let req = feature_ioctl(HIDIOCSFEATURE_NR, buf.len());
        let ret = retry_eintr(|| unsafe { libc::ioctl(self.fd, req as _, buf.as_ptr()) } as isize);
        if let Err(err) = ret {
            return Err(device_err("HIDIOCSFEATURE", &err));
        }
        Ok(())
    }

    fn get_feature(&self, buf: &mut [u8]) -> Result<usize> {
        let req = feature_ioctl(HIDIOCGFEATURE_NR, buf.len());
        let ret = match retry_eintr(|| unsafe { libc::ioctl(self.fd, req as _, buf.as_mut_ptr()) } as isize) {
            Ok(n) => n,
            Err(err) => return Err(device_err("HIDIOCGFEATURE", &err)),
        };
        // The hidraw driver's GFEATURE ioctl returns the number of bytes actually transferred —
        // the real short-read count, same contract as `HidD_GetFeature`'s length on Windows.
        Ok(ret as usize)
    }

    fn write_output(&self, buf: &[u8]) -> Result<()> {
        let n = match retry_eintr(|| unsafe { libc::write(self.fd, buf.as_ptr().cast(), buf.len()) }) {
            Ok(n) => n,
            Err(err) => return Err(device_err("write(2) (output report)", &err)),
        };
        // An output report is a single fixed-size frame — a real HID device never accepts a
        // partial one, so a short write here means the report did NOT land as sent. `write(2)` on
        // a character device can return fewer bytes than requested without erroring; trust the
        // count, not the non-negative return alone.
        if n as usize != buf.len() {
            bail!("write(2) (output report) short write: sent {n} of {} bytes", buf.len());
        }
        Ok(())
    }

    fn read_input(&self, buf: &mut [u8], timeout_ms: u32) -> Result<usize> {
        // Request/reply contract: a timeout here IS the caller's answer, not a spurious wakeup to
        // loop past — so `poll_read`'s `Ok(None)` becomes an honest error, mirroring
        // `WinHid::read_input`.
        match poll_read(self.fd, buf, timeout_ms.min(i32::MAX as u32) as i32)? {
            Some(n) => Ok(n),
            None => bail!("read_input timed out after {timeout_ms} ms"),
        }
    }
}

/// How long one [`HidRawReader::read`] waits for a report before returning `Ok(None)` and letting
/// the caller loop back around — mirrors `windows_hid::READER_POLL_TIMEOUT_MS`.
const READER_POLL_TIMEOUT_MS: i32 = 400;

/// A read-only handle for one collection's device-initiated input reports — a separate fd from
/// [`HidRaw`], opened `O_RDONLY`, so a long-lived listener thread never contends the control fd's
/// feature-report conversations.
pub struct HidRawReader {
    fd: RawFd,
}

impl HidRawReader {
    pub fn open(path: &DevicePath) -> Result<Self> {
        let dev_node = hidraw_dev_node(path)?;
        let cpath = CString::new(dev_node.clone())
            .map_err(|_| anyhow::anyhow!("device node path has an embedded NUL"))?;
        let fd = unsafe { libc::open(cpath.as_ptr(), libc::O_RDONLY | libc::O_CLOEXEC) };
        if fd < 0 {
            bail!("open {dev_node} (read) failed: {}", last_os_error());
        }
        Ok(HidRawReader { fd })
    }
}

impl Drop for HidRawReader {
    fn drop(&mut self) {
        unsafe {
            libc::close(self.fd);
        }
    }
}

impl InputReader for HidRawReader {
    fn read(&self, buf: &mut [u8]) -> Result<Option<usize>> {
        poll_read(self.fd, buf, READER_POLL_TIMEOUT_MS)
    }
}

// ── cross-process wire lock (flock) ───────────────────────────────────────────────────────────

/// Bounded wait for the cross-process flock — mirrors `windows_hid::WIRE_OS_WAIT_MS` and the same
/// reasoning: a legitimate conversation holds far less than this, so timing out means the foreign
/// holder is wedged and degrading to process-local-only is the safe choice.
const WIRE_FLOCK_WAIT_MS: u32 = 2000;
/// Poll interval while waiting for the flock — `flock(2)` has no built-in timeout, so the bounded
/// wait above is implemented as a short-sleep retry loop.
const WIRE_FLOCK_POLL_INTERVAL: Duration = Duration::from_millis(5);

fn lock_dir() -> PathBuf {
    match std::env::var_os("XDG_RUNTIME_DIR").filter(|v| !v.is_empty()) {
        Some(rt) => PathBuf::from(rt).join("neuron"),
        None => std::env::temp_dir().join("neuron"),
    }
}

/// The KERNEL half of a [`WireLock`] on Linux: an `flock(2)` on a well-known file under
/// `$XDG_RUNTIME_DIR/neuron/` (falling back to the system temp dir), named from a stable hash of
/// the `DevicePath` so every neuron process opening the same control pipe locks the same file.
/// `flock` locks are per OPEN FILE DESCRIPTION, not per process, so two processes (or two
/// independently-`open()`ed fds in one process — see the test below) contend the SAME lock exactly
/// the way `OsWireMutex`'s named kernel mutex does on Windows.
pub(super) struct OsWireFlock {
    fd: RawFd,
}

impl OsWireFlock {
    pub(super) fn for_path(path: &DevicePath) -> Option<OsWireFlock> {
        let hash = stable_hash(path.as_os_str().to_string_lossy().as_bytes());
        let dir = lock_dir();
        std::fs::create_dir_all(&dir).ok()?;
        Self::open_named(&dir.join(format!("wire-{hash:016x}.lock")))
    }

    /// Create-or-open the lock file and hold its fd (the flock is taken/released per conversation
    /// by [`acquire`](Self::acquire)/[`release`](Self::release), not for the fd's whole lifetime).
    /// `None` on any failure (unwritable runtime dir, exotic filesystem) — the caller degrades to
    /// process-local-only, never worse than the pre-cross-process behavior.
    pub(super) fn open_named(file: &Path) -> Option<OsWireFlock> {
        let cpath = CString::new(file.as_os_str().to_string_lossy().into_owned()).ok()?;
        let fd = unsafe {
            libc::open(
                cpath.as_ptr(),
                libc::O_RDWR | libc::O_CREAT | libc::O_CLOEXEC,
                0o600,
            )
        };
        if fd < 0 {
            None
        } else {
            Some(OsWireFlock { fd })
        }
    }

    /// Bounded acquire; `true` = held.
    pub(super) fn acquire(&self) -> bool {
        self.acquire_for(WIRE_FLOCK_WAIT_MS)
    }

    /// [`acquire`](Self::acquire) with an explicit wait budget — the production path always uses
    /// `WIRE_FLOCK_WAIT_MS`; tests use short budgets to prove blocking without stalling the suite.
    pub(super) fn acquire_for(&self, ms: u32) -> bool {
        let deadline = Instant::now() + Duration::from_millis(ms as u64);
        loop {
            let ret = unsafe { libc::flock(self.fd, libc::LOCK_EX | libc::LOCK_NB) };
            if ret == 0 {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(WIRE_FLOCK_POLL_INTERVAL);
        }
    }

    pub(super) fn release(&self) {
        unsafe {
            libc::flock(self.fd, libc::LOCK_UN);
        }
    }
}

impl Drop for OsWireFlock {
    fn drop(&mut self) {
        unsafe {
            libc::close(self.fd);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A feature ioctl blocks for a whole USB control transfer, so an interrupting signal must not
    /// surface as a failed device write. Drives the retry with a caller-set errno rather than a
    /// real signal, so the test is deterministic.
    #[test]
    fn retry_eintr_retries_only_while_the_errno_is_eintr() {
        let mut calls = 0;
        let out = retry_eintr(|| {
            calls += 1;
            unsafe { *libc::__errno_location() = libc::EINTR };
            if calls < 3 {
                -1
            } else {
                7
            }
        });
        assert_eq!(out.ok(), Some(7), "retries past EINTR and returns the eventual result");
        assert_eq!(calls, 3);

        let mut calls = 0;
        let out = retry_eintr(|| {
            calls += 1;
            unsafe { *libc::__errno_location() = libc::EIO };
            -1
        });
        assert_eq!(
            out.err().and_then(|e| e.raw_os_error()),
            Some(libc::EIO),
            "any other errno is returned, not retried"
        );
        assert_eq!(calls, 1);
    }

    /// A stalled request is not a missing device. A Seiren V3 Mini answering HIDIOCGFEATURE with
    /// EPIPE was reported as "device disconnected" while sitting plugged in and enumerating fine,
    /// which would have had callers dropping a present device.
    #[test]
    fn a_stall_is_not_a_disconnect() {
        assert!(is_disconnect(libc::ENODEV), "ENODEV: device was removed");
        assert!(is_disconnect(libc::ESHUTDOWN), "ESHUTDOWN: disabled, e.g. physical disconnect");
        assert!(!is_disconnect(libc::EPIPE), "EPIPE is a stall, not a removal");
        assert!(!is_disconnect(libc::EIO));

        let stall = device_err("HIDIOCGFEATURE", &std::io::Error::from_raw_os_error(libc::EPIPE)).to_string();
        assert!(stall.contains("refused the request"), "{stall}");
        assert!(!stall.contains("disconnected"), "{stall}");

        let gone = device_err("HIDIOCGFEATURE", &std::io::Error::from_raw_os_error(libc::ENODEV)).to_string();
        assert!(gone.contains("device disconnected"), "{gone}");
    }

    // ── pure parsing ───────────────────────────────────────────────────────────────────────────

    #[test]
    fn parses_hid_id_from_uevent() {
        let uevent = "DRIVER=hid-generic\nHID_ID=0003:00001532:0000005A\nHID_NAME=Razer Naga\n";
        assert_eq!(parse_hid_id(uevent), Some((0x0003, 0x1532, 0x005A)));
    }

    #[test]
    fn parses_hid_id_with_narrow_hex_fields() {
        // Real files pad vendor/product to 8 hex digits, but the parser shouldn't assume the
        // width — it splits on ':' and parses whatever's there.
        let uevent = "HID_ID=3:1532:a8\n";
        assert_eq!(parse_hid_id(uevent), Some((0x0003, 0x1532, 0x00A8)));
    }

    #[test]
    fn missing_hid_id_line_is_none() {
        assert_eq!(parse_hid_id("DRIVER=hid-generic\nHID_NAME=Foo\n"), None);
    }

    #[test]
    fn malformed_hid_id_is_none_not_a_panic() {
        assert_eq!(parse_hid_id("HID_ID=not-hex-at-all\n"), None);
        assert_eq!(parse_hid_id("HID_ID=0003:1532\n"), None); // missing product field
        assert_eq!(parse_hid_id(""), None);
    }

    #[test]
    fn encode_and_decode_round_trip() {
        let path = encode_path("/sys/devices/.../hidraw/hidraw3", 7);
        let raw = path.as_os_str().to_string_lossy();
        assert!(raw.ends_with("#col07"));
        let (sysfs, col) = decode_path(&raw).expect("decodes");
        assert_eq!(sysfs, "/sys/devices/.../hidraw/hidraw3");
        assert_eq!(col, 7);
    }

    #[test]
    fn decode_rejects_paths_without_the_suffix() {
        assert_eq!(decode_path("/sys/devices/.../hidraw/hidraw3"), None);
        assert_eq!(decode_path("/sys/devices/.../hidraw/hidraw3#col1"), None); // not 2 digits
        assert_eq!(decode_path("/sys/devices/.../hidraw/hidraw3#colxx"), None);
    }

    #[test]
    fn dev_node_from_encoded_path() {
        let path = encode_path("/sys/devices/pci0000:00/usb1/1-2/1-2:1.0/0003:1532:0091.0001/hidraw/hidraw2", 1);
        assert_eq!(hidraw_dev_node(&path).expect("resolves"), "/dev/hidraw2");
    }

    #[test]
    fn feature_ioctl_matches_the_kernel_uapi_header() {
        // include/uapi/linux/hidraw.h: HIDIOCGFEATURE(len) = _IOC(_IOC_WRITE|_IOC_READ,'H',0x07,len)
        // include/uapi/asm-generic/ioctl.h: _IOC(dir,type,nr,size) =
        //   (dir<<30)|(type<<8)|(nr<<0)|(size<<16)
        // For len=91 (the razer_report feature length): dir=3, type='H'=0x48, nr=0x07, size=91.
        let got = feature_ioctl(HIDIOCGFEATURE_NR, 91);
        let want: u32 = (3u32 << 30) | (91u32 << 16) | (0x48u32 << 8) | 0x07;
        assert_eq!(got, want as libc::c_ulong);
    }

    #[test]
    fn stable_hash_is_deterministic_and_path_sensitive() {
        assert_eq!(stable_hash(b"/dev/hidraw0"), stable_hash(b"/dev/hidraw0"));
        assert_ne!(stable_hash(b"/dev/hidraw0"), stable_hash(b"/dev/hidraw1"));
    }

    // ── the flock exclusion contract ──────────────────────────────────────────────────────────

    /// `flock` locks belong to the OPEN FILE DESCRIPTION, not the process — two independently
    /// `open()`ed fds onto the SAME path contend the lock exactly like two separate processes
    /// would, so this same-process test is a faithful proof of the cross-process guarantee (the
    /// same reasoning `windows_hid.rs`'s named-kernel-mutex test relies on for its two handles).
    #[test]
    fn flock_excludes_across_separate_opens_of_the_same_file() {
        let dir = std::env::temp_dir().join(format!("neuron-wire-selftest-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let file = dir.join("wire.lock");

        let a = OsWireFlock::open_named(&file).expect("first open");
        let b = OsWireFlock::open_named(&file).expect("second open");
        assert!(a.acquire(), "first handle acquires immediately");
        assert!(
            !b.acquire_for(100),
            "a second, independently-opened fd on the SAME file must NOT acquire while the \
             first holds — otherwise flock exclusion here is fiction"
        );
        a.release();
        assert!(
            b.acquire_for(WIRE_FLOCK_WAIT_MS),
            "once the first releases, the second must be able to acquire"
        );
        b.release();
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── enumeration over a synthetic sysfs tree ────────────────────────────────────────────────

    /// Build one hidraw node under `base`, shaped like real sysfs: the class entry is a symlink
    /// into the device tree, `hidrawN/device` is a symlink to the HID interface dir holding
    /// `uevent` + `report_descriptor`, and the USB device dir two levels up holds `product`.
    /// Returns the canonical path the walk should report for it.
    fn fake_hidraw_node(base: &Path, port: &str, node: &str, hid_id: &str, desc: &[u8], product: &str) -> PathBuf {
        use std::os::unix::fs::symlink;
        let usb_dev = base.join("devices").join(port);
        let iface = usb_dev.join(format!("0003:1532:005A.{port}"));
        let hidraw_dir = iface.join("hidraw").join(node);
        std::fs::create_dir_all(&hidraw_dir).expect("hidraw dir");
        std::fs::write(usb_dev.join("product"), format!("{product}\n")).expect("product");
        std::fs::write(iface.join("uevent"), format!("DRIVER=hid-generic\nHID_ID={hid_id}\n")).expect("uevent");
        std::fs::write(iface.join("report_descriptor"), desc).expect("report_descriptor");
        symlink("../..", hidraw_dir.join("device")).expect("device symlink");

        let class_root = base.join("class").join("hidraw");
        std::fs::create_dir_all(&class_root).expect("class root");
        symlink(&hidraw_dir, class_root.join(node)).expect("class symlink");
        std::fs::canonicalize(&hidraw_dir).expect("canonical hidraw dir")
    }

    /// Two top-level Application collections on one interface: a vendor page carrying a 90-byte
    /// feature report (the razer_report shape) and a Generic Desktop mouse carrying an input
    /// report. Written as raw bytes because the item builders live in `hid_descriptor`'s own
    /// tests.
    const TWO_COLLECTION_DESC: &[u8] = &[
        0x06, 0x00, 0xFF, // Usage Page (vendor 0xFF00)
        0x09, 0x01, // Usage (1)
        0xA1, 0x01, // Collection (Application)
        0x75, 0x08, //   Report Size (8)
        0x95, 0x5A, //   Report Count (90)
        0xB1, 0x02, //   Feature
        0xC0, // End Collection
        0x05, 0x01, // Usage Page (Generic Desktop)
        0x09, 0x02, // Usage (Mouse)
        0xA1, 0x01, // Collection (Application)
        0x75, 0x08, //   Report Size (8)
        0x95, 0x01, //   Report Count (1)
        0x81, 0x02, //   Input
        0xC0, // End Collection
    ];

    /// The enumeration walk end to end, against a synthetic sysfs tree: one interface per device,
    /// one `HidDeviceInfo` per top-level collection, report lengths from the descriptor, the
    /// product string read from the USB parent, and the collection index encoded into the path.
    /// This is the path that decides WHICH device a write reaches, and no hardware is needed to
    /// prove its shape.
    #[test]
    fn enumerates_a_synthetic_sysfs_tree() {
        let base = std::env::temp_dir().join(format!("neuron-sysfs-{}-a", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let canonical = fake_hidraw_node(&base, "1-2", "hidraw0", "0003:00001532:0000005A", TWO_COLLECTION_DESC, "Razer Naga V2 Pro");

        let found = enumerate_in(&base.join("class").join("hidraw")).expect("enumerate");
        assert_eq!(found.len(), 2, "one entry per top-level collection");

        let vendor = &found[0];
        assert_eq!((vendor.vid, vendor.pid), (0x1532, 0x005A));
        assert_eq!((vendor.usage_page, vendor.usage), (0xFF00, 0x0001));
        assert_eq!(vendor.feature_len, 91, "90 data bytes + the report-ID byte");
        assert_eq!(vendor.product, "Razer Naga V2 Pro", "product comes from the USB parent dir");
        assert_eq!(
            vendor.path,
            encode_path(&canonical.to_string_lossy(), 1),
            "the path is the CANONICAL device-tree path, not the class symlink"
        );

        let mouse = &found[1];
        assert_eq!((mouse.usage_page, mouse.usage), (0x0001, 0x0002));
        assert_eq!(mouse.input_len, 2);
        assert_eq!(mouse.path, encode_path(&canonical.to_string_lossy(), 2));
        assert_ne!(vendor.path, mouse.path, "collections of one interface stay addressable apart");

        let _ = std::fs::remove_dir_all(&base);
    }

    /// What the walk must SKIP: a non-USB/Bluetooth bus, and a node whose sysfs files are missing
    /// (an unplug racing the walk). Neither may fail the enumeration or emit an entry.
    #[test]
    fn enumeration_skips_other_buses_and_unreadable_nodes() {
        let base = std::env::temp_dir().join(format!("neuron-sysfs-{}-b", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let class_root = base.join("class").join("hidraw");

        // I2C (bus 0x0018) — a laptop touchpad, not something to talk razer_report at.
        fake_hidraw_node(&base, "1-3", "hidraw1", "0018:00001532:0000005A", TWO_COLLECTION_DESC, "I2C HID");
        assert!(enumerate_in(&class_root).expect("enumerate").is_empty(), "non-USB/BT buses are skipped");

        // A class entry with no readable device dir at all.
        std::fs::create_dir_all(class_root.join("hidraw9")).expect("bare node");
        assert!(
            enumerate_in(&class_root).expect("enumerate").is_empty(),
            "a node whose files can't be read is skipped, not an error"
        );

        // A missing class root is an empty bus, not a failure.
        assert!(enumerate_in(&base.join("class").join("nope")).expect("enumerate").is_empty());

        let _ = std::fs::remove_dir_all(&base);
    }

    // ── live checkpoint ────────────────────────────────────────────────────────────────────────

    /// Capture the Linux half of a HID parity fixture for every Razer device on this machine: the
    /// raw report descriptor from sysfs, plus what our parser makes of it. Pairs with the Windows
    /// half (`transport::windows_hid::tests::capture_parity_fixture`) so
    /// `transport::parity::tests::parser_agrees_with_windows_caps` can cross-check the two on every
    /// CI run afterwards, with no device attached.
    ///
    /// Reads `/sys` only — it opens no `/dev/hidraw*` node and writes nothing to any device.
    ///
    /// `cargo test -p neuron --lib transport::hidraw::tests::capture_parity_fixture -- --ignored --nocapture`
    #[test]
    #[ignore = "captures a fixture from the HID devices present on this machine"]
    fn capture_parity_fixture() {
        use crate::transport::parity::{encode_hex, Capture};
        const RAZER: u16 = 0x1532;

        let mut by_pid: std::collections::BTreeMap<u16, Capture> = std::collections::BTreeMap::new();
        let entries = std::fs::read_dir("/sys/class/hidraw").expect("no /sys/class/hidraw on this machine");
        for entry in entries.flatten() {
            let device_dir = entry.path().join("device");
            let Ok(uevent) = std::fs::read_to_string(device_dir.join("uevent")) else {
                continue;
            };
            let Some((_bus, vid, pid)) = parse_hid_id(&uevent) else {
                continue;
            };
            if vid != RAZER {
                continue;
            }
            let Ok(desc) = std::fs::read(device_dir.join("report_descriptor")) else {
                continue;
            };
            let product = std::fs::canonicalize(entry.path())
                .ok()
                .and_then(|p| usb_product_string(&p))
                .unwrap_or_default();

            let cap = by_pid.entry(pid).or_insert_with(|| Capture {
                source: "linux-report-descriptor".into(),
                vid,
                pid,
                product: product.clone(),
                report_descriptor_hex: Some(String::new()),
                collections: Vec::new(),
            });
            if cap.product.is_empty() {
                cap.product = product;
            }
            // One fixture per device, but a device has several hidraw interfaces, each with its own
            // descriptor. Concatenating them is exactly right for this purpose: descriptors are a
            // flat item stream, and the parser treats each top-level Application collection
            // independently, so the parse of the concatenation is the union of the parses — which
            // is the set Windows enumerates per device.
            if let Some(hex) = cap.report_descriptor_hex.as_mut() {
                hex.push_str(&encode_hex(&desc));
            }
            println!("{:?}: {} descriptor bytes", entry.path(), desc.len());
        }

        assert!(!by_pid.is_empty(), "no Razer devices present — nothing to capture");
        for (pid, cap) in by_pid.iter_mut() {
            cap.collections = cap.reparse().expect("the captured descriptor re-parses");
            let path = cap.write("linux").expect("write fixture");
            println!(
                "{pid:04x} {:<28} {} collection(s) -> {}",
                cap.product,
                cap.collections.len(),
                path.display()
            );
            for c in &cap.collections {
                println!(
                    "    {:#06x}/{:#06x}  feature={:<4} input={:<4} output={}",
                    c.usage_page, c.usage, c.feature_len, c.input_len, c.output_len
                );
            }
        }
    }

    /// Read one feature report from every Razer collection that declares one, and report the byte
    /// count the kernel actually transferred. The hardware checkpoint for `HIDIOCGFEATURE`: its
    /// return value is the real transferred length, and this is where that stops being a claim
    /// from a header file.
    ///
    /// A feature READ, never a write. A device that answers nothing is reported, not failed.
    ///
    /// `cargo test -p neuron --lib transport::hidraw::tests::live_feature_read -- --ignored --nocapture`
    #[test]
    #[ignore = "needs a Razer device present on this machine"]
    fn live_feature_read() {
        const RAZER: u16 = 0x1532;
        let _wire = crate::transport::allow_real_hardware();
        let devices = crate::transport::enumerate().expect("enumerate");
        let mut tried = 0;
        for d in devices.iter().filter(|d| d.vid == RAZER && d.feature_len > 1) {
            tried += 1;
            print!(
                "{:04x}:{:04x} {:#06x}/{:#06x} feature_len={} -> ",
                d.vid, d.pid, d.usage_page, d.usage, d.feature_len
            );
            match crate::transport::open_path(&d.path) {
                Ok(t) => {
                    let mut buf = vec![0u8; d.feature_len as usize];
                    match t.get_feature(&mut buf) {
                        Ok(n) => println!(
                            "HIDIOCGFEATURE returned {n} byte(s){}",
                            if n as u16 == d.feature_len { " (== feature_len)" } else { " (SHORT)" }
                        ),
                        Err(e) => println!("get_feature failed: {e}"),
                    }
                }
                Err(e) => println!("open failed: {e}"),
            }
        }
        assert!(tried > 0, "no Razer collection with a feature report — nothing to read");
    }

    /// LIVE enumeration probe: prints what this machine's `enumerate()` sees. For the hardware
    /// checkpoint — run with a real Razer device (or any HID device) plugged into WSL via
    /// `usbipd`/passthrough:
    /// `cargo test -p neuron --lib transport::hidraw::tests::live_enumerate -- --ignored --nocapture`
    #[test]
    #[ignore = "needs a HID device present on this machine"]
    fn live_enumerate() {
        let _wire = crate::transport::allow_real_hardware();
        match crate::transport::enumerate() {
            Ok(devices) => {
                println!("{} HID collection(s):", devices.len());
                for d in &devices {
                    println!(
                        "  vid={:04x} pid={:04x} usage_page={:#06x} usage={:#06x} \
                         feature_len={} input_len={} output_len={} product={:?} path={:?}",
                        d.vid,
                        d.pid,
                        d.usage_page,
                        d.usage,
                        d.feature_len,
                        d.input_len,
                        d.output_len,
                        d.product,
                        d.path.as_os_str()
                    );
                }
            }
            Err(e) => println!("enumerate() failed: {e}"),
        }
    }
}
