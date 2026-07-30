// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

//! Windows HID transport via the Win32 API (windows-sys). Mirrors the proven approach:
//! open the control collection with dwDesiredAccess = 0 (Windows blocks GENERIC_R/W on a
//! mouse, but HidD_Get/SetFeature use FILE_ANY_ACCESS IOCTLs, so access=0 works).

use super::{wire_lock_for, DevicePath, HidDeviceInfo, Transport, WireLock};
use anyhow::{bail, Result};
use std::ffi::c_void;
use std::ptr;
use std::sync::{Arc, Mutex};
use windows_sys::core::GUID;
use windows_sys::Win32::Security::{
    InitializeSecurityDescriptor, SetSecurityDescriptorDacl, ACL, SECURITY_ATTRIBUTES,
    SECURITY_DESCRIPTOR,
};
use windows_sys::Win32::System::Threading::{CreateMutexW, ReleaseMutex};
use windows_sys::Win32::Devices::DeviceAndDriverInstallation::{
    SetupDiDestroyDeviceInfoList, SetupDiEnumDeviceInterfaces, SetupDiGetClassDevsW,
    SetupDiGetDeviceInterfaceDetailW, DIGCF_DEVICEINTERFACE, DIGCF_PRESENT,
    SP_DEVICE_INTERFACE_DATA, SP_DEVICE_INTERFACE_DETAIL_DATA_W,
};
use windows_sys::Win32::Devices::HumanInterfaceDevice::{
    HidD_FreePreparsedData, HidD_GetAttributes, HidD_GetFeature, HidD_GetHidGuid,
    HidD_GetPreparsedData, HidD_GetProductString, HidD_SetFeature, HidP_GetCaps,
    HIDD_ATTRIBUTES, HIDP_CAPS,
};
use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};

const HIDP_OK: i32 = 0x0011_0000; // HIDP_STATUS_SUCCESS
const GENERIC_READ_FLAG: u32 = 0x8000_0000; // GENERIC_READ — declared locally to dodge windows-sys path churn
const GENERIC_WRITE_FLAG: u32 = 0x4000_0000; // GENERIC_WRITE — output reports need it (WriteFile)
const FILE_FLAG_OVERLAPPED: u32 = 0x4000_0000; // async I/O flag (dwFlagsAndAttributes slot, not access)
const ERROR_IO_PENDING: u32 = 997; // ReadFile returned 0 but the overlapped op is in flight
const WAIT_OBJECT_0: u32 = 0; // WaitForSingleObject: the event signaled (read completed)
const WAIT_ABANDONED_0: u32 = 0x80; // WaitForSingleObject: mutex holder DIED — ownership transferred to us
const SECURITY_DESCRIPTOR_REVISION: u32 = 1; // the only revision Win32 has ever defined
/// Bounded wait for the cross-process wire mutex: a conversation legitimately holds ≤~600ms
/// worst-case (the ACK'd 60×10ms poll against a non-answering device), so 2s of waiting means the
/// foreign holder is wedged — proceed with only the process-local lock rather than deadlock a
/// user's command behind a hung process (see `super::WireLock`'s degradation rules).
const WIRE_OS_WAIT_MS: u32 = 2000;

// The Win32 OVERLAPPED control block for an async ReadFile. `Win32_System_IO` is NOT in this
// crate's windows-sys feature set (see Cargo.toml — frozen manifest), so the struct AND its
// helpers (CancelIo/GetOverlappedResult) are hand-declared here, exactly as `ReadFile` already is.
// Layout matches the Win32 `OVERLAPPED` (the Offset/OffsetHigh union arm — we never use Pointer).
#[repr(C)]
struct Overlapped {
    internal: usize,
    internal_high: usize,
    offset: u32,
    offset_high: u32,
    h_event: HANDLE,
}

// These aren't exported under this windows-sys feature set; declare them directly. They live in
// kernel32, which this crate already links (CreateFileW et al.), so the symbols resolve. (Same
// escape hatch the pre-existing `ReadFile` declaration uses.)
#[link(name = "kernel32")]
extern "system" {
    fn ReadFile(
        handle: HANDLE,
        buf: *mut c_void,
        len: u32,
        read: *mut u32,
        overlapped: *mut c_void,
    ) -> i32;
    fn WriteFile(
        handle: HANDLE,
        buf: *const c_void,
        len: u32,
        written: *mut u32,
        overlapped: *mut c_void,
    ) -> i32;
    fn CreateEventW(
        attrs: *const c_void,
        manual_reset: i32,
        initial_state: i32,
        name: *const u16,
    ) -> HANDLE;
    fn WaitForSingleObject(handle: HANDLE, ms: u32) -> u32;
    fn CancelIo(handle: HANDLE) -> i32;
    fn GetOverlappedResult(
        handle: HANDLE,
        overlapped: *mut c_void,
        transferred: *mut u32,
        wait: i32,
    ) -> i32;
    fn GetLastError() -> u32;
}

/// One overlapped `ReadFile` against `handle`, bounded by `timeout_ms`. Shared by every read path
/// in this file (`WinHid::read_input`'s request/reply probe AND `WinHidReader::read`'s listener
/// loop) so the cancellation/lifetime contract is written and audited in exactly one place.
///
/// Returns `Ok(Some(n))` on a completed read, `Ok(None)` on timeout (nothing arrived in the
/// window — NOT an error), `Err` on a real I/O failure.
///
/// SAFETY / lifetime contract: `buf` and the stack-local `Overlapped` (`ov`) are both handed to
/// the kernel by pointer for the duration of the I/O, so NEITHER may be dropped, moved, or reused
/// while a read is in flight — that would be a use-after-free the kernel writes into after this
/// function has already returned. Every exit path below upholds this:
///   - `ReadFile` completes synchronously (`started != 0`): no I/O is pending — safe to return.
///   - `ReadFile` fails outright (`started == 0` and the error isn't `ERROR_IO_PENDING`): nothing
///     was queued — safe to return.
///   - The wait succeeds (`WAIT_OBJECT_0`): the event only signals when the kernel has finished
///     writing into `buf`/`ov` — the subsequent `GetOverlappedResult(..., wait=0)` is a
///     non-blocking formality to fetch the byte count, not a second wait.
///   - The wait TIMES OUT: this is the dangerous path — the kernel may still be about to write
///     into `buf`/`ov` at any instant. We call `CancelIo` (cancels I/O the CALLING THREAD issued
///     on this handle; both callers here issue and await from the same thread, so `CancelIo`
///     fully covers it — unlike `CancelIoEx`'s cross-thread cancel, unneeded here) and then
///     `GetOverlappedResult(..., wait=TRUE)`, which BLOCKS until the kernel confirms the I/O has
///     actually stopped (cancellation is asynchronous — `CancelIo` returning is not proof the op
///     is done). Only once that call returns do we close the event and return `Ok(None)`; only
///     then are `buf`/`ov` safe for the caller to drop or reuse. This holds even if the cancel
///     "fails" (e.g. the read had already completed the instant before `CancelIo` ran) —
///     `GetOverlappedResult(wait=TRUE)` still blocks until the kernel is done with the buffer
///     either way, so there is no path out of this function with I/O still pending.
/// A panic between `ReadFile` and this function's return would unwind through the same code path
/// (Rust doesn't skip drops/cleanup here — there IS no separate cleanup to skip, since this
/// function contains no early-return before the cancel-and-reap sequence completes; the only heap
/// object involved, `ev`, is a plain `HANDLE` closed on every exit, including the timeout path).
unsafe fn overlapped_read_timed(handle: HANDLE, buf: &mut [u8], timeout_ms: u32) -> Result<Option<usize>> {
    // Manual-reset, initially non-signaled event for the overlapped completion.
    let ev = CreateEventW(ptr::null(), 1, 0, ptr::null());
    if ev.is_null() {
        bail!("CreateEvent for overlapped read failed");
    }
    let mut ov: Overlapped = std::mem::zeroed();
    ov.h_event = ev;
    let mut got: u32 = 0;
    let ov_ptr = &mut ov as *mut Overlapped as *mut c_void;
    let started = ReadFile(
        handle,
        buf.as_mut_ptr() as *mut c_void,
        buf.len() as u32,
        &mut got,
        ov_ptr,
    );
    if started == 0 {
        let err = GetLastError();
        if err != ERROR_IO_PENDING {
            CloseHandle(ev);
            bail!("ReadFile (overlapped) failed: {err}");
        }
        // In flight: wait the caller's bounded patience.
        if WaitForSingleObject(ev, timeout_ms) != WAIT_OBJECT_0 {
            // Timed out. Cancel and REAP the op (so `buf`/`ov` are safe to drop) before returning —
            // a dangling overlapped read into this buffer is a use-after-free. `GetOverlappedResult`
            // with `wait=TRUE` blocks until the kernel confirms the cancellation actually landed.
            CancelIo(handle);
            let _ = GetOverlappedResult(handle, ov_ptr, &mut got, 1 /* bWait */);
            CloseHandle(ev);
            return Ok(None);
        }
        if GetOverlappedResult(handle, ov_ptr, &mut got, 0) == 0 {
            CloseHandle(ev);
            bail!("GetOverlappedResult (overlapped read) failed");
        }
    }
    CloseHandle(ev);
    Ok(Some(got as usize))
}

unsafe fn wide_from_ptr(p: *const u16) -> Vec<u16> {
    let mut v = Vec::new();
    let mut i = 0isize;
    loop {
        let c = *p.offset(i);
        v.push(c);
        if c == 0 {
            break;
        }
        i += 1;
    }
    v
}

unsafe fn query(path: &[u16]) -> Option<HidDeviceInfo> {
    let h = CreateFileW(
        path.as_ptr(),
        0, // query only — works on protected mouse/keyboard collections
        FILE_SHARE_READ | FILE_SHARE_WRITE,
        ptr::null(),
        OPEN_EXISTING,
        0,
        ptr::null_mut(),
    );
    if h == INVALID_HANDLE_VALUE {
        return None;
    }
    let mut info = None;
    let mut attr: HIDD_ATTRIBUTES = std::mem::zeroed();
    attr.Size = std::mem::size_of::<HIDD_ATTRIBUTES>() as u32;
    if HidD_GetAttributes(h, &mut attr) != 0 {
        let mut pp: isize = 0; // PHIDP_PREPARSED_DATA is an opaque isize in windows-sys
        if HidD_GetPreparsedData(h, &mut pp) != 0 {
            let mut caps: HIDP_CAPS = std::mem::zeroed();
            if HidP_GetCaps(pp, &mut caps) == HIDP_OK {
                // Product string (IOCTL_HID_GET_PRODUCT_STRING is FILE_ANY_ACCESS, so it works
                // on this access-0 handle like Get/SetFeature). Best-effort: empty on failure.
                let mut prod = [0u16; 127];
                let product = if HidD_GetProductString(
                    h,
                    prod.as_mut_ptr() as *mut c_void,
                    (prod.len() * 2) as u32,
                ) != 0
                {
                    let end = prod.iter().position(|&c| c == 0).unwrap_or(prod.len());
                    String::from_utf16_lossy(&prod[..end]).trim().to_string()
                } else {
                    String::new()
                };
                info = Some(HidDeviceInfo {
                    vid: attr.VendorID,
                    pid: attr.ProductID,
                    usage_page: caps.UsagePage,
                    usage: caps.Usage,
                    feature_len: caps.FeatureReportByteLength,
                    // The second wire surface's shape, from the SAME HIDP_CAPS the feature len comes
                    // from — a HID++ family recognizes its 7/20-byte output/input reports by these.
                    input_len: caps.InputReportByteLength,
                    output_len: caps.OutputReportByteLength,
                    // `path` is the NUL-terminated wide buffer from `wide_from_ptr`; store it as the
                    // opaque key (NUL stripped) — `WinHid::open` re-adds it via `to_wide_nul`.
                    path: DevicePath::from_wide(path),
                    product,
                });
            }
            HidD_FreePreparsedData(pp);
        }
    }
    CloseHandle(h);
    info
}

pub fn enumerate() -> Result<Vec<HidDeviceInfo>> {
    let mut out = Vec::new();
    unsafe {
        let mut guid: GUID = std::mem::zeroed();
        HidD_GetHidGuid(&mut guid);
        let set = SetupDiGetClassDevsW(
            &guid,
            ptr::null(),
            ptr::null_mut(),
            DIGCF_PRESENT | DIGCF_DEVICEINTERFACE,
        );
        if set == -1 {
            // HDEVINFO is an isize here; failure is INVALID_HANDLE_VALUE (-1).
            bail!("SetupDiGetClassDevs failed");
        }
        let mut idx = 0u32;
        loop {
            let mut ifa: SP_DEVICE_INTERFACE_DATA = std::mem::zeroed();
            ifa.cbSize = std::mem::size_of::<SP_DEVICE_INTERFACE_DATA>() as u32;
            if SetupDiEnumDeviceInterfaces(set, ptr::null_mut(), &guid, idx, &mut ifa) == 0 {
                break;
            }
            idx += 1;

            let mut req = 0u32;
            SetupDiGetDeviceInterfaceDetailW(
                set,
                &ifa,
                ptr::null_mut(),
                0,
                &mut req,
                ptr::null_mut(),
            );
            if req == 0 {
                continue;
            }
            let mut buf = vec![0u8; req as usize];
            let detail = buf.as_mut_ptr() as *mut SP_DEVICE_INTERFACE_DETAIL_DATA_W;
            // cbSize is the size of the fixed header: 8 on 64-bit, 6 on 32-bit.
            (*detail).cbSize = if cfg!(target_pointer_width = "64") {
                8
            } else {
                6
            };
            if SetupDiGetDeviceInterfaceDetailW(set, &ifa, detail, req, &mut req, ptr::null_mut())
                == 0
            {
                continue;
            }
            let path_ptr = ptr::addr_of!((*detail).DevicePath) as *const u16;
            let path = wide_from_ptr(path_ptr);
            if let Some(info) = query(&path) {
                out.push(info);
            }
        }
        SetupDiDestroyDeviceInfoList(set);
    }
    Ok(out)
}

pub struct WinHid {
    handle: HANDLE,
    /// Whether `handle` was opened with GENERIC_WRITE. The feature-report path (Get/SetFeature) is
    /// FILE_ANY_ACCESS and works at access 0 either way; only `write_output` (a real WriteFile)
    /// needs write access, so this lets it error HONESTLY when we only got the access-0 fallback.
    can_write: bool,
    /// Kept so `read_input` can lazily open its OWN overlapped read handle on first use (the trait
    /// method takes `&self`). The proven feature-report handle can't do a timed read.
    path: DevicePath,
    /// Lazily-opened GENERIC_READ + FILE_FLAG_OVERLAPPED handle for `read_input`. `Mutex` gives the
    /// interior mutability the `&self` trait method needs; `WinHid` is single-threaded per `Device`
    /// so the lock is uncontended. `None` until the first `read_input`.
    read_handle: Mutex<Option<HANDLE>>,
    /// The pipe's shared WIRE LOCK, resolved via [`wire_lock_for`] on `path` — every `WinHid`
    /// opened on the same `DevicePath` (a separate handle from a separate in-process actor) gets
    /// the IDENTICAL `Arc`, so a `Dialect`'s conversation-holding guard serializes them; the
    /// lock's kernel half ([`OsWireMutex`]) extends the same guarantee across PROCESSES (see
    /// `Transport::wire_lock`).
    wire: Arc<WireLock>,
}

impl WinHid {
    pub fn open(path: &DevicePath) -> Result<Self> {
        // Re-add the NUL terminator `CreateFileW` requires (centralized in `to_wide_nul`). This
        // reproduces the exact wide buffer the enumeration path passed to `CreateFileW`.
        let wide = path.to_wide_nul();
        unsafe {
            // Prefer GENERIC_READ|GENERIC_WRITE: output reports (`write_output`) need write access.
            // Windows denies R/W on a protected mouse/keyboard control collection, so FALL BACK to
            // access 0 — the razer feature-report IOCTLs are FILE_ANY_ACCESS and are unaffected
            // either way. `can_write` records which handle we ended up with.
            let mut can_write = true;
            let mut h = CreateFileW(
                wide.as_ptr(),
                GENERIC_READ_FLAG | GENERIC_WRITE_FLAG,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                ptr::null(),
                OPEN_EXISTING,
                0,
                ptr::null_mut(),
            );
            if h == INVALID_HANDLE_VALUE {
                can_write = false;
                h = CreateFileW(
                    wide.as_ptr(),
                    0,
                    FILE_SHARE_READ | FILE_SHARE_WRITE,
                    ptr::null(),
                    OPEN_EXISTING,
                    0,
                    ptr::null_mut(),
                );
            }
            if h == INVALID_HANDLE_VALUE {
                bail!("CreateFile on control interface failed");
            }
            Ok(WinHid {
                handle: h,
                can_write,
                path: path.clone(),
                read_handle: Mutex::new(None),
                wire: wire_lock_for(path),
            })
        }
    }
}

impl Drop for WinHid {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.handle);
            // Close the lazily-opened overlapped read handle too, if `read_input` ever opened one.
            if let Some(rh) = *self.read_handle.lock().unwrap_or_else(std::sync::PoisonError::into_inner) {
                CloseHandle(rh);
            }
        }
    }
}

/// A read handle for one collection's device-initiated input reports. Opened with `GENERIC_READ`
/// (which `ReadFile` needs) — so it FAILS on the OS-protected mouse/keyboard collections and only
/// succeeds on the vendor collections where Razer's event reports (DPI/stage changes) ride.
///
/// Opened `FILE_FLAG_OVERLAPPED` so `read` can enforce [`READER_POLL_TIMEOUT_MS`]: a synchronous
/// read on this handle would block a listener thread FOREVER if the device stalls mid-session
/// (wireless dropout, sleep transition, driver hiccup) — unplug happens to complete the pending
/// IO, but nothing else does. The bounded overlapped read means the thread always wakes up on its
/// own schedule to re-issue the read (see `overlapped_read_timed`'s cancellation contract).
pub struct WinHidReader {
    handle: HANDLE,
}

// The handle is a raw OS pointer; we own it solely here and close it on drop, so it's safe to move
// to the listener thread that owns this reader.
unsafe impl Send for WinHidReader {}

/// How long one `WinHidReader::read` waits for a report before returning `Ok(None)` and letting
/// the caller loop back around (check a stop flag, re-issue). Short enough that a listener thread
/// stays responsive to shutdown/re-arm; long enough that idle devices don't spin the thread.
const READER_POLL_TIMEOUT_MS: u32 = 400;

impl WinHidReader {
    pub fn open(path: &DevicePath) -> Result<Self> {
        let wide = path.to_wide_nul();
        unsafe {
            let h = CreateFileW(
                wide.as_ptr(),
                GENERIC_READ_FLAG,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                ptr::null(),
                OPEN_EXISTING,
                FILE_FLAG_OVERLAPPED,
                ptr::null_mut(),
            );
            if h == INVALID_HANDLE_VALUE {
                bail!("CreateFile (read) failed — collection is OS-protected or busy");
            }
            Ok(WinHidReader { handle: h })
        }
    }
}

impl super::InputReader for WinHidReader {
    fn read(&self, buf: &mut [u8]) -> Result<Option<usize>> {
        // One bounded overlapped read; see `overlapped_read_timed` for the cancellation/lifetime
        // contract. `Ok(None)` (timeout) is NOT an error — the caller's loop is expected to spin
        // back around and call `read` again, which is exactly what re-issues the ReadFile.
        unsafe { overlapped_read_timed(self.handle, buf, READER_POLL_TIMEOUT_MS) }
    }
}

impl Drop for WinHidReader {
    fn drop(&mut self) {
        // No overlapped read is ever left pending across calls (each `read` cancels-and-reaps its
        // own timeout before returning — see `overlapped_read_timed`), so there is nothing to
        // cancel here; a plain close is safe.
        unsafe {
            CloseHandle(self.handle);
        }
    }
}

/// The KERNEL half of a [`WireLock`]: a named Win32 mutex shared by every process on this login
/// session that opens the same control pipe — the app's host writer and a `neuron-cli` command
/// resolve the identical kernel object by name, so their request/reply conversations serialize
/// exactly like two in-process handles do, closing the cross-process race.
pub(super) struct OsWireMutex {
    handle: HANDLE,
}

// SAFETY: the HANDLE is a kernel-object reference; WaitForSingleObject/ReleaseMutex are
// thread-safe entry points. The Win32 rule that a mutex must be RELEASED by the thread that
// acquired it is enforced structurally: `WireGuard` holds a `MutexGuard` and is therefore !Send,
// so acquire and release can never land on different threads.
unsafe impl Send for OsWireMutex {}
unsafe impl Sync for OsWireMutex {}

impl OsWireMutex {
    /// The named mutex for a device pipe. The name must be STABLE ACROSS PROCESSES, so it's an
    /// FNV-1a hash of the path's UTF-16 units — NOT `DefaultHasher`, whose SipHash keys are
    /// randomized per process (two processes would derive two different names and never meet).
    /// `Local\` namespace = this login session, the only place two neuron processes coexist
    /// (and it needs no privilege, unlike `Global\`).
    pub(super) fn for_path(path: &DevicePath) -> Option<OsWireMutex> {
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        for unit in path.to_wide_nul() {
            for b in unit.to_le_bytes() {
                h = (h ^ b as u64).wrapping_mul(0x0000_0100_0000_01b3);
            }
        }
        Self::open_named(&format!("Local\\neuron-wire-{h:016x}"))
    }

    /// Create-or-open the named mutex with an EXPLICIT NULL DACL (everyone full access). This is
    /// load-bearing, not laziness: with NULL security attributes the object inherits the creator
    /// TOKEN's default DACL, and an elevated process's token owner is BUILTIN\Administrators — a
    /// default-DACL mutex created by the elevated tray app would be unopenable by an unelevated
    /// `neuron-cli` (whose filtered token lacks Administrators), silently killing the
    /// cross-process guarantee in exactly the deployment it exists for (elevated tray + normal
    /// shell). A world-accessible mutex is a safe object to leave open: the worst a hostile local
    /// process can do is HOLD it, and the bounded wait in `WireLock::acquire` caps that at a 2s
    /// delay before degrading to local-only — a nuisance, not a lockout. `None` on any create
    /// failure → the caller runs process-local, never worse than the pre-kernel-layer behavior.
    pub(super) fn open_named(name: &str) -> Option<OsWireMutex> {
        unsafe {
            let mut sd: SECURITY_DESCRIPTOR = std::mem::zeroed();
            let psd = &mut sd as *mut SECURITY_DESCRIPTOR as *mut c_void;
            if InitializeSecurityDescriptor(psd, SECURITY_DESCRIPTOR_REVISION) == 0 {
                return None;
            }
            // present=TRUE + dacl=NULL is the documented "NULL DACL" (allow everyone) shape —
            // distinct from "no DACL present", which would fall back to the default DACL.
            if SetSecurityDescriptorDacl(psd, 1, ptr::null::<ACL>(), 0) == 0 {
                return None;
            }
            let sa = SECURITY_ATTRIBUTES {
                nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
                lpSecurityDescriptor: psd,
                bInheritHandle: 0,
            };
            let wide: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
            // bInitialOwner = FALSE: creating must not implicitly acquire — acquisition is
            // exclusively WireLock::acquire's job, or the create-path would deadlock itself.
            let handle = CreateMutexW(&sa, 0, wide.as_ptr());
            if handle.is_null() {
                None
            } else {
                Some(OsWireMutex { handle })
            }
        }
    }

    /// Bounded acquire; `true` = held. `WAIT_ABANDONED` counts as held: the previous holder DIED
    /// mid-conversation, ownership transferred to us, and the reply-echo filter already tolerates
    /// whatever half-conversation the corpse left on the pipe (the same story as a same-process
    /// crash before this layer existed). Timeout/failure = `false` → the caller proceeds with only
    /// the process-local lock (see `WIRE_OS_WAIT_MS` for why that's the right failure).
    pub(super) fn acquire(&self) -> bool {
        self.acquire_for(WIRE_OS_WAIT_MS)
    }

    /// [`acquire`](Self::acquire) with an explicit wait budget — the production path always uses
    /// `WIRE_OS_WAIT_MS`; tests use short budgets to PROVE blocking (a must-time-out probe while
    /// another process holds the mutex) without stalling the suite.
    pub(super) fn acquire_for(&self, ms: u32) -> bool {
        unsafe {
            matches!(
                WaitForSingleObject(self.handle, ms),
                WAIT_OBJECT_0 | WAIT_ABANDONED_0
            )
        }
    }

    pub(super) fn release(&self) {
        unsafe {
            ReleaseMutex(self.handle);
        }
    }
}

impl Drop for OsWireMutex {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.handle);
        }
    }
}

impl Transport for WinHid {
    fn wire_lock(&self) -> Option<Arc<WireLock>> {
        Some(self.wire.clone())
    }

    fn set_feature(&self, buf: &[u8]) -> Result<()> {
        unsafe {
            if HidD_SetFeature(self.handle, buf.as_ptr() as *const c_void, buf.len() as u32) == 0 {
                bail!("HidD_SetFeature failed");
            }
        }
        Ok(())
    }
    fn get_feature(&self, buf: &mut [u8]) -> Result<()> {
        unsafe {
            if HidD_GetFeature(
                self.handle,
                buf.as_mut_ptr() as *mut c_void,
                buf.len() as u32,
            ) == 0
            {
                bail!("HidD_GetFeature failed");
            }
        }
        Ok(())
    }

    fn write_output(&self, buf: &[u8]) -> Result<()> {
        // Output reports are a real WriteFile, which needs the GENERIC_WRITE handle. If `open` only
        // got the access-0 fallback (protected collection), say so instead of silently no-op'ing —
        // the razer feature-report path is unaffected, but HID++ cannot ride this collection.
        if !self.can_write {
            bail!("output report needs a GENERIC_WRITE handle; this collection opened access-0 only");
        }
        unsafe {
            let mut written: u32 = 0;
            if WriteFile(
                self.handle,
                buf.as_ptr() as *const c_void,
                buf.len() as u32,
                &mut written,
                ptr::null_mut(),
            ) == 0
            {
                bail!("WriteFile (output report) failed");
            }
        }
        Ok(())
    }

    fn read_input(&self, buf: &mut [u8], timeout_ms: u32) -> Result<usize> {
        // A timed input-report read. The control handle is synchronous (a blocking ReadFile could
        // hang a probe thread forever if the reply never comes), so we use a SEPARATE handle opened
        // GENERIC_READ + FILE_FLAG_OVERLAPPED and enforce the timeout with WaitForSingleObject +
        // CancelIo. Opened lazily on first use (many devices never speak the output/input surface)
        // and cached in `read_handle`. Contained here — the proven feature-report path never sees it.
        let mut slot = self.read_handle.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if slot.is_none() {
            let wide = self.path.to_wide_nul();
            unsafe {
                let rh = CreateFileW(
                    wide.as_ptr(),
                    GENERIC_READ_FLAG,
                    FILE_SHARE_READ | FILE_SHARE_WRITE,
                    ptr::null(),
                    OPEN_EXISTING,
                    FILE_FLAG_OVERLAPPED,
                    ptr::null_mut(),
                );
                if rh == INVALID_HANDLE_VALUE {
                    bail!("CreateFile (overlapped input read) failed — collection is OS-protected or busy");
                }
                *slot = Some(rh);
            }
        }
        let rh = slot.expect("read handle opened just above");
        drop(slot); // HANDLE is Copy — don't hold the lock across the blocking wait
        // `read_input`'s contract (unlike `InputReader::read`) is request/reply: "did the reply
        // arrive in the window" — a timeout here IS the caller's answer, not a spurious wakeup to
        // loop past. So the shared helper's `Ok(None)` is turned back into an honest error.
        match unsafe { overlapped_read_timed(rh, buf, timeout_ms) }? {
            Some(n) => Ok(n),
            None => bail!("read_input timed out after {timeout_ms} ms"),
        }
    }
}
