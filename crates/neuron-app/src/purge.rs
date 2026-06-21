//! Synapse purge — evict Razer's resident slopware by PROVENANCE and the PROCESS TREE, not by a
//! hand-maintained list of names.
//!
//! The old approach matched a hardcoded set of service keys and exe-name fragments ("razer", "rzsdk",
//! …). That loses both ways: it misses anything Razer renames or ships under a generic name, and it
//! can't follow the helpers a process spawns off. So this is the "truth" rewrite:
//!
//!   * THE TRUTH OF "IS THIS RAZER" IS THE IMAGE PATH. Razer installs its entire stack under
//!     directories literally named `Razer` (or `Razer <something>`): `…\Program Files (x86)\Razer\`,
//!     `…\ProgramData\Razer\`, `…\AppData\Local\Razer\`, `…\Razer Chroma SDK\`. A process whose
//!     executable resolves under such a directory IS Razer's, whatever the exe is called. We read the
//!     full path with `QueryFullProcessImageNameW` — no name guessing.
//!
//!   * THE TREE IS EXPLORED, NOT GUESSED. From every Razer-pathed *root* we walk the parent→child
//!     graph (Toolhelp gives `th32ParentProcessID`) and mark the whole subtree — so a helper Razer
//!     spawned, even one living outside a Razer folder, is caught as a descendant of a known rat.
//!
//!   * SERVICES ARE DISCOVERED, NOT LISTED. We `EnumServicesStatusExW` the entire SCM and keep the
//!     ones whose binary path is under a Razer directory (or whose display name says "Razer"). Those
//!     are the respawn engine — we DISABLE them (start-type → disabled, so they can't come back now or
//!     after a reboot) and then stop them, before sweeping the processes.
//!
//! Order matters: disable+stop the services first (kills the respawn engine and a disabled service
//! won't restart from a kill), then snapshot and `TerminateProcess` the rat tree, twice, to catch
//! anything mid-spawn when the first snapshot was taken.
//!
//! Razer services run as SYSTEM, so a full purge needs elevation; if we're not elevated we relaunch
//! ourselves via UAC (`runas` + `--purge-synapse`). All native Win32 (windows-sys) — we don't fight
//! slopware by shelling out to `sc`/`taskkill`/PowerShell.

#![cfg(windows)]

use std::collections::{HashMap, HashSet, VecDeque};
use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;

use windows_sys::Win32::Foundation::{
    CloseHandle, GetLastError, ERROR_SERVICE_NOT_ACTIVE, FALSE, HANDLE, INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::Security::{
    GetTokenInformation, TokenElevation, TOKEN_ELEVATION, TOKEN_QUERY,
};
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
};
use windows_sys::Win32::System::Services::{
    ChangeServiceConfigW, CloseServiceHandle, ControlService, EnumServicesStatusExW, OpenSCManagerW,
    OpenServiceW, QueryServiceConfigW, QueryServiceStatus, ENUM_SERVICE_STATUS_PROCESSW, QUERY_SERVICE_CONFIGW,
    SC_ENUM_PROCESS_INFO, SC_MANAGER_CONNECT, SC_MANAGER_ENUMERATE_SERVICE, SERVICE_CHANGE_CONFIG,
    SERVICE_CONTROL_STOP, SERVICE_DISABLED, SERVICE_NO_CHANGE, SERVICE_QUERY_CONFIG,
    SERVICE_QUERY_STATUS, SERVICE_RUNNING, SERVICE_STATE_ALL, SERVICE_STATUS, SERVICE_STOP,
    SERVICE_STOP_PENDING, SERVICE_WIN32,
};
use windows_sys::Win32::System::Threading::{
    GetCurrentProcess, OpenProcess, OpenProcessToken, QueryFullProcessImageNameW, TerminateProcess,
    PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_TERMINATE,
};

/// Outcome of a purge attempt, surfaced to the status line.
pub enum Outcome {
    /// Purged in-process (we were elevated). Counts feed the status line.
    Done {
        killed: u32,
        stopped: u32,
        disabled: u32,
    },
    /// Not elevated — a UAC'd second instance was launched to do the kill. The user sees the prompt.
    Elevating,
    /// Couldn't even start (UAC declined, or self-path unknown).
    Failed(&'static str),
}

/// Wide-encode a Rust str as a NUL-terminated UTF-16 buffer for Win32 W APIs.
fn wide(s: &str) -> Vec<u16> {
    OsStr::new(s)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

/// Read a NUL-terminated wide pointer (as the SCM hands us inside its enum buffer) into a String.
unsafe fn pwstr(p: *const u16) -> String {
    if p.is_null() {
        return String::new();
    }
    let mut len = 0isize;
    while *p.offset(len) != 0 {
        len += 1;
    }
    String::from_utf16_lossy(std::slice::from_raw_parts(p, len as usize))
}

/// Are we running elevated (full admin token)? Service-stop + SYSTEM-process kill need it.
pub fn is_elevated() -> bool {
    unsafe {
        let mut token: HANDLE = std::ptr::null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
            return false;
        }
        let mut elev = TOKEN_ELEVATION { TokenIsElevated: 0 };
        let mut ret_len = 0u32;
        let ok = GetTokenInformation(
            token,
            TokenElevation,
            &mut elev as *mut _ as *mut _,
            std::mem::size_of::<TOKEN_ELEVATION>() as u32,
            &mut ret_len,
        );
        CloseHandle(token);
        ok != 0 && elev.TokenIsElevated != 0
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// PROVENANCE — the one piece of truth everything keys off
// ─────────────────────────────────────────────────────────────────────────────

/// Does this filesystem path live inside a Razer-owned directory? Razer's whole install footprint sits
/// under a directory named `Razer` or `Razer <thing>` (Razer Chroma SDK, Razer Services, Razer Synapse
/// 3…). We test path COMPONENTS, so a `\Razer\` anywhere in the chain is a hit while a stray user folder
/// like `razerfan\` is not. Quotes/args on a service's binary path don't matter — the clean `Razer`
/// component still falls out of the split.
fn is_razer_path(path: &str) -> bool {
    path.split(|c| c == '\\' || c == '/').any(|seg| {
        let s = seg.trim().trim_matches('"').to_ascii_lowercase();
        s == "razer" || s.starts_with("razer ")
    })
}

// ─────────────────────────────────────────────────────────────────────────────
// PROCESSES — snapshot, then the rat TREE
// ─────────────────────────────────────────────────────────────────────────────

struct Proc {
    pid: u32,
    ppid: u32,
    path: Option<String>,
}

/// A process marked for the kill, with why (a Razer-pathed `root`, or a `child` it spawned).
pub struct Rat {
    pub pid: u32,
    pub path: String,
    pub root: bool,
}

/// Full Win32 image path of a process, or None if we can't open/query it (access denied, gone).
fn image_path(pid: u32) -> Option<String> {
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, FALSE, pid);
        if h.is_null() || h == INVALID_HANDLE_VALUE {
            return None;
        }
        let mut buf = [0u16; 1024];
        let mut len = buf.len() as u32;
        // dwflags 0 = PROCESS_NAME_WIN32 (drive-letter path, not the \Device\… native form).
        let ok = QueryFullProcessImageNameW(h, 0, buf.as_mut_ptr(), &mut len);
        CloseHandle(h);
        if ok != 0 && len > 0 {
            Some(String::from_utf16_lossy(&buf[..len as usize]))
        } else {
            None
        }
    }
}

/// One Toolhelp pass: every live process with its parent and (best-effort) full image path.
fn snapshot() -> Vec<Proc> {
    let mut out = Vec::new();
    unsafe {
        let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snap == INVALID_HANDLE_VALUE {
            return out;
        }
        let mut e: PROCESSENTRY32W = std::mem::zeroed();
        e.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
        if Process32FirstW(snap, &mut e) != 0 {
            loop {
                let pid = e.th32ProcessID;
                out.push(Proc {
                    pid,
                    ppid: e.th32ParentProcessID,
                    path: image_path(pid),
                });
                if Process32NextW(snap, &mut e) == 0 {
                    break;
                }
            }
        }
        CloseHandle(snap);
    }
    out
}

/// The rat set: every Razer-pathed process (a `root`), plus the entire subtree each one spawned.
/// Ourselves, the kernel pseudo-pids (0/4), and any second copy of our own exe are never included.
fn find_rats(procs: &[Proc]) -> Vec<Rat> {
    let me = std::process::id();
    let my_exe = std::env::current_exe()
        .ok()
        .map(|p| p.to_string_lossy().to_ascii_lowercase());
    let protected = |pid: u32, path: &Option<String>| -> bool {
        if pid == me || pid == 0 || pid == 4 {
            return true;
        }
        match (path, &my_exe) {
            (Some(p), Some(mine)) => p.to_ascii_lowercase() == *mine,
            _ => false,
        }
    };

    // parent → children, and pid → path, for the tree walk.
    let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
    let mut path_of: HashMap<u32, Option<String>> = HashMap::new();
    for p in procs {
        children.entry(p.ppid).or_default().push(p.pid);
        path_of.insert(p.pid, p.path.clone());
    }

    let mut roots: HashSet<u32> = HashSet::new();
    for p in procs {
        if protected(p.pid, &p.path) {
            continue;
        }
        if matches!(&p.path, Some(path) if is_razer_path(path)) {
            roots.insert(p.pid);
        }
    }

    // BFS the spawn tree from every root, collecting descendants too.
    let mut all: HashSet<u32> = roots.clone();
    let mut q: VecDeque<u32> = roots.iter().copied().collect();
    while let Some(pid) = q.pop_front() {
        if let Some(kids) = children.get(&pid) {
            for &k in kids {
                let kp = path_of.get(&k).cloned().flatten();
                if protected(k, &kp) {
                    continue;
                }
                if all.insert(k) {
                    q.push_back(k);
                }
            }
        }
    }

    let mut rats: Vec<Rat> = all
        .into_iter()
        .map(|pid| Rat {
            pid,
            path: path_of
                .get(&pid)
                .cloned()
                .flatten()
                .unwrap_or_else(|| "<unresolved>".into()),
            root: roots.contains(&pid),
        })
        .collect();
    // roots first, then by pid — a stable, readable order for the scan log.
    rats.sort_by(|a, b| b.root.cmp(&a.root).then(a.pid.cmp(&b.pid)));
    rats
}

/// TerminateProcess one pid; true if it actually died at our hand.
fn terminate(pid: u32) -> bool {
    unsafe {
        let h = OpenProcess(PROCESS_TERMINATE, FALSE, pid);
        if h.is_null() || h == INVALID_HANDLE_VALUE {
            return false;
        }
        let ok = TerminateProcess(h, 1) != 0;
        CloseHandle(h);
        ok
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// SERVICES — enumerate the whole SCM, keep Razer's, disable + stop them
// ─────────────────────────────────────────────────────────────────────────────

/// A discovered Razer service. `key` is what OpenServiceW wants; the rest is for the report.
pub struct Svc {
    pub key: String,
    pub display: String,
    pub bin: String,
    pub running: bool,
}

/// The service's launch command (binary path, possibly quoted with args), via QueryServiceConfigW.
unsafe fn service_bin(svc: HANDLE) -> Option<String> {
    let mut needed = 0u32;
    // First call sizes the buffer (it fails with ERROR_INSUFFICIENT_BUFFER and sets `needed`).
    QueryServiceConfigW(svc, std::ptr::null_mut(), 0, &mut needed);
    if needed == 0 {
        return None;
    }
    let mut buf = vec![0u8; needed as usize];
    if QueryServiceConfigW(
        svc,
        buf.as_mut_ptr() as *mut QUERY_SERVICE_CONFIGW,
        needed,
        &mut needed,
    ) == 0
    {
        return None;
    }
    let cfg = &*(buf.as_ptr() as *const QUERY_SERVICE_CONFIGW);
    Some(pwstr(cfg.lpBinaryPathName))
}

/// Enumerate every Win32 service and keep the Razer ones — decided by binary-path provenance OR a
/// "Razer" display/key name (the brand it stamps on its own services). No hardcoded service list.
fn discover_services(scm: HANDLE) -> Vec<Svc> {
    let mut found = Vec::new();
    unsafe {
        // Two-call EnumServicesStatusExW: size, then fill. One pass suffices — pcbBytesNeeded is total.
        let mut needed = 0u32;
        let mut count = 0u32;
        let mut resume = 0u32;
        EnumServicesStatusExW(
            scm,
            SC_ENUM_PROCESS_INFO,
            SERVICE_WIN32,
            SERVICE_STATE_ALL,
            std::ptr::null_mut(),
            0,
            &mut needed,
            &mut count,
            &mut resume,
            std::ptr::null(),
        );
        if needed == 0 {
            return found;
        }
        let mut buf = vec![0u8; needed as usize];
        if EnumServicesStatusExW(
            scm,
            SC_ENUM_PROCESS_INFO,
            SERVICE_WIN32,
            SERVICE_STATE_ALL,
            buf.as_mut_ptr(),
            buf.len() as u32,
            &mut needed,
            &mut count,
            &mut resume,
            std::ptr::null(),
        ) == 0
        {
            return found;
        }
        let entries =
            std::slice::from_raw_parts(buf.as_ptr() as *const ENUM_SERVICE_STATUS_PROCESSW, count as usize);
        for e in entries {
            let key = pwstr(e.lpServiceName);
            let display = pwstr(e.lpDisplayName);
            let name_says_razer =
                key.to_ascii_lowercase().contains("razer") || display.to_ascii_lowercase().contains("razer");

            // Path is the stronger truth — open the service just to read its binary path.
            let wkey = wide(&key);
            let bin = {
                let h = OpenServiceW(scm, wkey.as_ptr(), SERVICE_QUERY_CONFIG);
                if h.is_null() {
                    None
                } else {
                    let b = service_bin(h);
                    CloseServiceHandle(h);
                    b
                }
            };
            let path_says_razer = bin.as_deref().map(is_razer_path).unwrap_or(false);

            if name_says_razer || path_says_razer {
                found.push(Svc {
                    key,
                    display,
                    bin: bin.unwrap_or_default(),
                    running: e.ServiceStatusProcess.dwCurrentState == SERVICE_RUNNING,
                });
            }
        }
    }
    found
}

/// Disable (start-type → disabled, so it can't restart now or on reboot) then gracefully stop one
/// service. Returns (stopped, disabled). Needs elevation to bite on a SYSTEM service.
fn neutralize_service(scm: HANDLE, key: &str) -> (bool, bool) {
    unsafe {
        let wkey = wide(key);
        let svc = OpenServiceW(
            scm,
            wkey.as_ptr(),
            SERVICE_STOP | SERVICE_QUERY_STATUS | SERVICE_CHANGE_CONFIG,
        );
        if svc.is_null() {
            return (false, false);
        }
        // DISABLE FIRST: a disabled service won't be auto-restarted by the SCM when we stop/kill it,
        // and it stays gone across reboots. SERVICE_NO_CHANGE leaves type/error-control untouched.
        let disabled = ChangeServiceConfigW(
            svc,
            SERVICE_NO_CHANGE,
            SERVICE_DISABLED,
            SERVICE_NO_CHANGE,
            std::ptr::null(),
            std::ptr::null(),
            std::ptr::null_mut(),
            std::ptr::null(),
            std::ptr::null(),
            std::ptr::null(),
            std::ptr::null(),
        ) != 0;

        // STOP gracefully through the SCM (not a TerminateProcess — a clean stop won't trip a
        // failure-action restart), then wait out STOP_PENDING so its children are gone before the sweep.
        let mut status: SERVICE_STATUS = std::mem::zeroed();
        let rc = ControlService(svc, SERVICE_CONTROL_STOP, &mut status);
        let mut stopped = rc != 0;
        if rc == 0 && GetLastError() == ERROR_SERVICE_NOT_ACTIVE {
            stopped = true; // already down — fine
        }
        if stopped {
            for _ in 0..20 {
                // keep waiting only while it's actively STOP_PENDING; bail once stopped or wedged.
                if QueryServiceStatus(svc, &mut status) != 0
                    && status.dwCurrentState == SERVICE_STOP_PENDING
                {
                    std::thread::sleep(std::time::Duration::from_millis(100));
                } else {
                    break;
                }
            }
        }
        CloseServiceHandle(svc);
        (stopped, disabled)
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// ORCHESTRATION
// ─────────────────────────────────────────────────────────────────────────────

/// The full purge: disable+stop Razer services (the respawn engine), then sweep the rat process tree
/// twice. Returns (killed, stopped, disabled). Requires elevation for SYSTEM services/processes.
fn do_purge() -> (u32, u32, u32) {
    let (mut stopped, mut disabled) = (0u32, 0u32);
    unsafe {
        let scm = OpenSCManagerW(
            std::ptr::null(),
            std::ptr::null(),
            SC_MANAGER_CONNECT | SC_MANAGER_ENUMERATE_SERVICE,
        );
        if !scm.is_null() {
            for s in discover_services(scm) {
                let (st, di) = neutralize_service(scm, &s.key);
                stopped += st as u32;
                disabled += di as u32;
            }
            CloseServiceHandle(scm);
        }
    }

    // Sweep the process tree twice — the second pass catches anything that was mid-spawn (or a child
    // a dying service handed off) when the first snapshot was read.
    let mut killed = 0u32;
    for pass in 0..2 {
        let procs = snapshot();
        for r in find_rats(&procs) {
            if terminate(r.pid) {
                killed += 1;
            }
        }
        if pass == 0 {
            std::thread::sleep(std::time::Duration::from_millis(150));
        }
    }
    (killed, stopped, disabled)
}

/// Relaunch ourselves elevated (UAC) with `flag`. Returns false if we couldn't.
fn relaunch_elevated(flag: &str) -> bool {
    unsafe {
        let exe = match std::env::current_exe() {
            Ok(p) => p,
            Err(_) => return false,
        };
        let exe_w = wide(&exe.to_string_lossy());
        let verb = wide("runas");
        let params = wide(flag);
        let h = windows_sys::Win32::UI::Shell::ShellExecuteW(
            std::ptr::null_mut(),
            verb.as_ptr(),
            exe_w.as_ptr(),
            params.as_ptr(),
            std::ptr::null(),
            windows_sys::Win32::UI::WindowsAndMessaging::SW_HIDE,
        );
        (h as isize) > 32 // ShellExecuteW returns >32 on success
    }
}

/// Button entry point (called from glue on the UI thread). Purges inline if elevated; otherwise fires
/// a UAC'd elevated instance to do it (Razer services are SYSTEM).
pub fn request() -> Outcome {
    if is_elevated() {
        let (killed, stopped, disabled) = do_purge();
        Outcome::Done {
            killed,
            stopped,
            disabled,
        }
    } else if relaunch_elevated("--purge-synapse") {
        Outcome::Elevating
    } else {
        Outcome::Failed("UAC declined — run Neuron as admin to purge Synapse")
    }
}

/// Entry point for the elevated `--purge-synapse` instance launched from `main()`. Purges, writes a
/// breadcrumb next to the exe, and exits — it never opens the GUI.
pub fn run_purge_and_log() {
    use std::io::Write as _;
    let (killed, stopped, disabled) = do_purge();
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open("neuron-purge.log")
    {
        let _ = writeln!(
            f,
            "[purge] disabled {disabled} + stopped {stopped} Razer service(s), terminated {killed} process(es)"
        );
    }
}

/// Non-destructive PREVIEW (`--scan-synapse`): discover the rat process tree + Razer services and
/// write them to `neuron-synapse-scan.log` (and stdout, for the debug console) WITHOUT killing or
/// disabling anything. Validates the detection on a live machine. Unelevated misses SYSTEM process
/// paths but still lists every Razer service.
pub fn scan_and_log() {
    use std::io::Write as _;
    let procs = snapshot();
    let rats = find_rats(&procs);
    let scm = unsafe {
        OpenSCManagerW(
            std::ptr::null(),
            std::ptr::null(),
            SC_MANAGER_CONNECT | SC_MANAGER_ENUMERATE_SERVICE,
        )
    };
    let svcs = if scm.is_null() {
        Vec::new()
    } else {
        let v = discover_services(scm);
        unsafe { CloseServiceHandle(scm) };
        v
    };

    let mut report = String::new();
    report.push_str(&format!(
        "[scan] elevated={}  rat-processes={}  razer-services={}\n",
        is_elevated(),
        rats.len(),
        svcs.len()
    ));
    report.push_str("-- PROCESSES (would TerminateProcess) --\n");
    for r in &rats {
        report.push_str(&format!(
            "  {:<5}  pid={:<6}  {}\n",
            if r.root { "ROOT" } else { "child" },
            r.pid,
            r.path
        ));
    }
    report.push_str("-- SERVICES (would disable + stop) --\n");
    for s in &svcs {
        report.push_str(&format!(
            "  {:<7}  {}  [{}]  {}\n",
            if s.running { "RUNNING" } else { "stopped" },
            s.key,
            s.display,
            s.bin
        ));
    }

    print!("{report}");
    let _ = std::io::stdout().flush();
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open("neuron-synapse-scan.log")
    {
        let _ = f.write_all(report.as_bytes());
    }
}
