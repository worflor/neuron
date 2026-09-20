// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

//! The profiler's 1 Hz logger (Windows). Spawned at startup ONLY when `NEURON_PROFILE` is set.
//!
//! Each second it records: neuron-core's hot-path [`neuron::prof`] counters as per-second DELTAS,
//! this process's per-thread CPU (top burners, named via `GetThreadDescription`), and the Python
//! sidecar's CPU. Output goes to `neuron_profile.log` (in the run dir) and stderr. A "never stops"
//! bug reads directly off the log: a counter that keeps climbing while the app is idle, or a
//! thread / the sidecar pinned at high CPU after a beacon when nothing should be running.

#[cfg(windows)]
pub fn start() {
    if !neuron::prof::enabled() {
        return;
    }
    crate::worker::spawn_detached("neuron-prof", run);
}

#[cfg(not(windows))]
pub fn start() {}

#[cfg(windows)]
fn run() {
    use neuron::prof;
    use std::fmt::Write as _;
    use std::io::Write;
    use std::sync::atomic::Ordering;

    let mut prev: Vec<u64> = prof::COUNTERS.iter().map(|(_, c)| c.load(Ordering::Relaxed)).collect();
    let mut prev_thr = thread_cpu();
    let mut prev_sc = process_cpu(prof::SIDECAR_PID.load(Ordering::Relaxed));
    eprintln!("[prof] NEURON_PROFILE on — 1Hz counters + per-thread/sidecar CPU -> neuron_profile.log");

    let mut t = 0u64;
    loop {
        std::thread::sleep(std::time::Duration::from_secs(1));
        t += 1;
        let mut line = format!("[prof {t:>4}s]");

        // hot-path counter deltas
        for (i, (label, c)) in prof::COUNTERS.iter().enumerate() {
            let now = c.load(Ordering::Relaxed);
            let d = now.wrapping_sub(prev[i]);
            prev[i] = now;
            if d > 0 {
                let _ = write!(line, " {label}={d}");
            }
        }

        // per-thread CPU deltas — the top burners (a spin shows here even if it's uninstrumented)
        let now_thr = thread_cpu();
        let mut burn: Vec<(u32, String, u64)> = now_thr
            .iter()
            .map(|(tid, (name, ticks))| {
                let p = prev_thr.get(tid).map_or(0, |(_, v)| *v);
                (*tid, name.clone(), ticks.saturating_sub(p))
            })
            .collect();
        burn.sort_by_key(|entry| std::cmp::Reverse(entry.2));
        prev_thr = now_thr;
        for (tid, name, d) in burn.iter().take(5) {
            let pct = (*d as f64) / 1e7; // 100ns ticks accrued over ~1s -> fraction of one core
            if pct >= 0.01 {
                let who = if name.is_empty() { format!("tid{tid}") } else { name.clone() };
                let _ = write!(line, " [{who} {:.0}%]", pct * 100.0);
            }
        }

        // cast-trigger / mouse-button state: which VKs read "down" right now (GetAsyncKeyState).
        // A trigger stuck "down" after a beacon is what would re-activate the weave on a loop.
        let down: Vec<i32> = [0x01, 0x02, 0x04, 0x05, 0x06]
            .into_iter()
            .filter(|&vk| neuron::glyph::key_down(vk))
            .collect();
        if !down.is_empty() {
            let _ = write!(line,
                " DOWN[{}]",
                down.iter().map(|vk| format!("vk{vk:#x}")).collect::<Vec<_>>().join(",")
            );
        }

        // the Python sidecar's CPU (a spin that lives in the macro host, not in Rust)
        let pid = prof::SIDECAR_PID.load(Ordering::Relaxed);
        if pid != 0 {
            let now_sc = process_cpu(pid);
            let d = now_sc.saturating_sub(prev_sc);
            prev_sc = now_sc;
            let pct = (d as f64) / 1e7;
            if pct >= 0.01 {
                let _ = write!(line, " [sidecar(py {pid}) {:.0}%]", pct * 100.0);
            }
        }

        eprintln!("{line}");
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(neuron::runroot::run_root().join("neuron_profile.log"))
        {
            let _ = writeln!(f, "{line}");
        }
    }
}

#[cfg(windows)]
fn ft(f: windows_sys::Win32::Foundation::FILETIME) -> u64 {
    (u64::from(f.dwHighDateTime) << 32) | u64::from(f.dwLowDateTime)
}

/// Map of tid -> (thread name, cumulative kernel+user CPU in 100ns ticks) for THIS process.
#[cfg(windows)]
fn thread_cpu() -> std::collections::HashMap<u32, (String, u64)> {
    use windows_sys::Win32::Foundation::{CloseHandle, FILETIME, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Thread32First, Thread32Next, TH32CS_SNAPTHREAD, THREADENTRY32,
    };
    use windows_sys::Win32::System::Threading::{
        GetCurrentProcessId, GetThreadTimes, OpenThread, THREAD_QUERY_INFORMATION,
    };

    let mut map = std::collections::HashMap::new();
    unsafe {
        let me = GetCurrentProcessId();
        let snap = CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0);
        if snap == INVALID_HANDLE_VALUE {
            return map;
        }
        let mut te: THREADENTRY32 = std::mem::zeroed();
        te.dwSize = std::mem::size_of::<THREADENTRY32>() as u32;
        let mut ok = Thread32First(snap, &raw mut te);
        while ok != 0 {
            if te.th32OwnerProcessID == me {
                let h = OpenThread(THREAD_QUERY_INFORMATION, 0, te.th32ThreadID);
                if !h.is_null() {
                    let (mut c, mut e, mut k, mut u): (FILETIME, FILETIME, FILETIME, FILETIME) =
                        std::mem::zeroed();
                    if GetThreadTimes(h, &raw mut c, &raw mut e, &raw mut k, &raw mut u) != 0 {
                        map.insert(te.th32ThreadID, (thread_name(h), ft(k) + ft(u)));
                    }
                    CloseHandle(h);
                }
            }
            ok = Thread32Next(snap, &raw mut te);
        }
        CloseHandle(snap);
    }
    map
}

/// Best-effort thread name via `GetThreadDescription` (the code names its worker threads). The
/// returned buffer is `LocalAlloc`'d and must be released with `LocalFree`.
#[cfg(windows)]
unsafe fn thread_name(h: windows_sys::Win32::Foundation::HANDLE) -> String {
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::System::Threading::GetThreadDescription;
    let mut p: *mut u16 = std::ptr::null_mut();
    if GetThreadDescription(h, &raw mut p) >= 0 && !p.is_null() {
        let mut len = 0;
        while *p.add(len) != 0 {
            len += 1;
        }
        let name = String::from_utf16_lossy(std::slice::from_raw_parts(p, len));
        LocalFree(p.cast());
        return name;
    }
    String::new()
}

/// Cumulative kernel+user CPU (100ns ticks) for another process (the sidecar), or 0 if unreadable.
#[cfg(windows)]
fn process_cpu(pid: u32) -> u64 {
    if pid == 0 {
        return 0;
    }
    use windows_sys::Win32::Foundation::{CloseHandle, FILETIME};
    use windows_sys::Win32::System::Threading::{
        GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if h.is_null() {
            return 0;
        }
        let (mut c, mut e, mut k, mut u): (FILETIME, FILETIME, FILETIME, FILETIME) =
            std::mem::zeroed();
        let r = if GetProcessTimes(h, &raw mut c, &raw mut e, &raw mut k, &raw mut u) != 0 {
            ft(k) + ft(u)
        } else {
            0
        };
        CloseHandle(h);
        r
    }
}
