// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

//! Resident-citizenship budget lane — run the real binary inside a Windows Job Object,
//! sample its resource footprint through scripted phases, and hold it to explicit budgets.
//!
//! A Job Object makes the child-process census exact (everything the app spawns lands in
//! the job too; a disarmed tray launch should have no sidecar),
//! and teardown is `TerminateJobObject` - one call, no orphans, even if the app wedges. The
//! real binary is used because allocator behavior, Slint/GPU surfaces, timers, and thread
//! spawns only exist in the shipped artifact.
//!
//! Every run writes what it observed (a JSON the caller can diff or archive) and asserts
//! only the ceilings in [`Budgets`], which carry headroom over the recorded hardware
//! baseline. Tightening a ceiling is a deliberate act, not drift.

#![cfg(windows)]

use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};

use std::collections::HashMap;
use std::os::windows::process::CommandExt as _;

use windows_sys::Win32::Foundation::{CloseHandle, FILETIME, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, Thread32First, Thread32Next,
    PROCESSENTRY32W, THREADENTRY32, TH32CS_SNAPPROCESS, TH32CS_SNAPTHREAD,
};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, QueryInformationJobObject,
    JobObjectBasicProcessIdList, JOBOBJECT_BASIC_PROCESS_ID_LIST, TerminateJobObject,
};
use windows_sys::Win32::System::ProcessStatus::{
    K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS,
};
use windows_sys::Win32::System::Threading::{
    GetProcessHandleCount, GetProcessIoCounters, GetProcessTimes, OpenProcess, OpenThread,
    ResumeThread, IO_COUNTERS, CREATE_SUSPENDED, PROCESS_QUERY_LIMITED_INFORMATION,
    THREAD_SUSPEND_RESUME,
};

/// Ceilings the resident app must stay under. Fields are generous over the recorded
/// baseline so CI noise doesn't flake, while regressions a user would feel still fail.
#[derive(Clone, Debug, serde::Serialize)]
pub struct Budgets {
    /// Max idle CPU as a percentage of TOTAL machine CPU (all cores).
    pub idle_cpu_pct_of_total: f64,
    /// Max private commit, bytes.
    pub private_bytes: u64,
    /// Max open handles.
    pub handles: u32,
    /// Max threads.
    pub threads: u32,
    /// Process image names allowed to exist in the job beside the app itself.
    pub allowed_children: &'static [&'static str],
}

impl Default for Budgets {
    fn default() -> Self {
        Budgets {
            idle_cpu_pct_of_total: 2.5,
            private_bytes: 400 * 1024 * 1024,
            handles: 900,
            threads: 48,
            allowed_children: &[],
        }
    }
}

/// One phase's observed footprint.
#[derive(Clone, Debug, serde::Serialize)]
pub struct Sample {
    pub phase: String,
    pub seconds: f64,
    /// CPU as a percentage of TOTAL machine CPU, summed over the app AND every descendant
    /// (macro-host sidecars count — they are part of the resident footprint on the user's box).
    pub cpu_pct_of_total: f64,
    /// Private commit summed over the app + descendants.
    pub private_bytes: u64,
    /// Working set summed over the app + descendants.
    pub working_set_bytes: u64,
    /// Process I/O includes filesystem, devices, and sockets; these are window deltas.
    pub io_read_bytes: u64,
    pub io_write_bytes: u64,
    pub io_read_ops: u64,
    pub io_write_ops: u64,
    /// Open handles summed over the app + descendants.
    pub handles: u32,
    /// Threads summed over the app + descendants.
    pub threads: u32,
    /// Image names of every descendant process (children census — an unexpected one is a finding).
    pub children: Vec<String>,
}

/// The app under measurement, alive inside its job.
pub struct Resident {
    job: HANDLE,
    child: std::process::Child,
}

// HANDLEs are process-local kernel references; the struct is only ever driven from the
// test thread, but Send lets harness helpers move it into scopes/threads safely.
unsafe impl Send for Resident {}

impl Resident {
    /// Launch `exe` (with args) inside a fresh Job Object, with no race window for a child to
    /// escape: the app is created suspended, assigned to the job before it runs a single
    /// instruction, then resumed. Once a process is in a job every process it spawns joins
    /// automatically, so `TerminateJobObject` on Drop reaps the whole tree with no orphans.
    /// The exe must not already be running outside the job.
    pub fn launch(exe: &Path, args: &[&str]) -> Result<Resident> {
        let job = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
        if job.is_null() {
            bail!("CreateJobObjectW failed");
        }
        let child = std::process::Command::new(exe)
            .args(args)
            .current_dir(exe.parent().context("exe has a parent dir")?)
            .creation_flags(CREATE_SUSPENDED)
            .spawn()
            .with_context(|| format!("launch {}", exe.display()))?;
        let fail = |mut child: std::process::Child, msg: &str| -> anyhow::Error {
            let _ = child.kill();
            unsafe { CloseHandle(job) };
            anyhow::anyhow!("{msg}")
        };
        if unsafe { AssignProcessToJobObject(job, child_handle(&child)) } == 0 {
            return Err(fail(
                child,
                "AssignProcessToJobObject failed (is the exe already elevated or in another job?)",
            ));
        }
        // Resume the (single, suspended) main thread now that the job owns the process.
        if let Err(e) = resume_process_main_thread(child.id()) {
            return Err(fail(child, &format!("resume suspended app: {e}")));
        }
        Ok(Resident { job, child })
    }

    #[must_use]
    pub fn pid(&self) -> u32 {
        self.child.id()
    }

    /// Sample the WHOLE resident footprint over `window`: the app plus every descendant
    /// (the sidecar counts). CPU is the summed kernel+user delta across the window normalized
    /// to all cores; memory/handles/threads are summed at the window's end.
    pub fn sample(&mut self, phase: &str, window: Duration) -> Result<Sample> {
        // CPU delta must survive a sidecar respawn mid-window: capture t0 CPU per pid, and at
        // t1 credit each pid only its OWN delta (a pid new at t1 contributes 0 this window).
        let initial_pids = job_process_ids(self.job)?;
        let cpu0: HashMap<u32, f64> = initial_pids
            .iter()
            .map(|&p| (p, proc_cpu_seconds_by_pid(p).unwrap_or(0.0)))
            .collect();
        let io0: HashMap<u32, IO_COUNTERS> = initial_pids
            .iter()
            .filter_map(|&p| proc_io_by_pid(p).map(|io| (p, io)))
            .collect();
        let t0 = Instant::now();
        std::thread::sleep(window);
        let dt = t0.elapsed().as_secs_f64();
        let cores = std::thread::available_parallelism()
            .map_or(1.0, |n| n.get() as f64);

        let (threads, children, family) = census(self.job, self.pid())?;
        if !family.contains(&self.pid()) {
            bail!("app pid {} vanished during the sample window", self.pid());
        }
        let mut cpu_delta = 0.0;
        let mut private_bytes = 0u64;
        let mut working_set_bytes = 0u64;
        let (mut io_read_bytes, mut io_write_bytes) = (0u64, 0u64);
        let (mut io_read_ops, mut io_write_ops) = (0u64, 0u64);
        let mut handles = 0u32;
        for &p in &family {
            let now = proc_cpu_seconds_by_pid(p).unwrap_or(0.0);
            cpu_delta += now - cpu0.get(&p).copied().unwrap_or(now); // new pid → delta 0
            if let Some((priv_b, ws, h)) = proc_mem_handles(p) {
                private_bytes += priv_b;
                working_set_bytes += ws;
                handles += h;
            }
            if let Some(now) = proc_io_by_pid(p) {
                let before = io0.get(&p).unwrap_or(&now);
                io_read_bytes += now.ReadTransferCount.saturating_sub(before.ReadTransferCount);
                io_write_bytes += now.WriteTransferCount.saturating_sub(before.WriteTransferCount);
                io_read_ops += now.ReadOperationCount.saturating_sub(before.ReadOperationCount);
                io_write_ops += now.WriteOperationCount.saturating_sub(before.WriteOperationCount);
            }
        }

        Ok(Sample {
            phase: phase.to_string(),
            seconds: dt,
            cpu_pct_of_total: cpu_delta / dt * 100.0 / cores,
            private_bytes,
            working_set_bytes,
            io_read_bytes,
            io_write_bytes,
            io_read_ops,
            io_write_ops,
            handles,
            threads,
            children,
        })
    }

    /// Assert one sample against the budgets; the error names every violated ceiling.
    pub fn assert_within(&self, s: &Sample, b: &Budgets) -> Result<()> {
        let mut violations = Vec::new();
        if s.cpu_pct_of_total > b.idle_cpu_pct_of_total {
            violations.push(format!(
                "cpu {:.2}% > budget {:.2}%",
                s.cpu_pct_of_total, b.idle_cpu_pct_of_total
            ));
        }
        if s.private_bytes > b.private_bytes {
            violations.push(format!(
                "private {} MB > budget {} MB",
                s.private_bytes / 1_048_576,
                b.private_bytes / 1_048_576
            ));
        }
        if s.handles > b.handles {
            violations.push(format!("handles {} > budget {}", s.handles, b.handles));
        }
        if s.threads > b.threads {
            violations.push(format!("threads {} > budget {}", s.threads, b.threads));
        }
        for c in &s.children {
            if !b.allowed_children.iter().any(|a| a.eq_ignore_ascii_case(c)) {
                violations.push(format!("unexpected child process '{c}'"));
            }
        }
        if violations.is_empty() {
            Ok(())
        } else {
            bail!("[{}] budget violations: {}", s.phase, violations.join("; "))
        }
    }
}

impl Drop for Resident {
    fn drop(&mut self) {
        // One call reaps the app AND everything it spawned — no orphans on any panic path.
        unsafe { TerminateJobObject(self.job, 0) };
        let _ = self.child.wait();
        unsafe { CloseHandle(self.job) };
    }
}

fn child_handle(child: &std::process::Child) -> HANDLE {
    use std::os::windows::io::AsRawHandle as _;
    child.as_raw_handle()
}

/// A queryable handle to another process (our own descendants). None if it exited.
struct ProcHandle(HANDLE);
impl ProcHandle {
    fn open(pid: u32) -> Option<ProcHandle> {
        let h = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
        if h.is_null() {
            None
        } else {
            Some(ProcHandle(h))
        }
    }
}
impl Drop for ProcHandle {
    fn drop(&mut self) {
        unsafe { CloseHandle(self.0) };
    }
}

fn proc_cpu_seconds_by_pid(pid: u32) -> Option<f64> {
    let h = ProcHandle::open(pid)?;
    let mut creation: FILETIME = unsafe { std::mem::zeroed() };
    let mut exit: FILETIME = unsafe { std::mem::zeroed() };
    let mut kernel: FILETIME = unsafe { std::mem::zeroed() };
    let mut user: FILETIME = unsafe { std::mem::zeroed() };
    if unsafe { GetProcessTimes(h.0, &raw mut creation, &raw mut exit, &raw mut kernel, &raw mut user) } == 0 {
        return None;
    }
    let ft = |f: FILETIME| (u64::from(f.dwHighDateTime) << 32 | u64::from(f.dwLowDateTime)) as f64 * 1e-7;
    Some(ft(kernel) + ft(user))
}

fn proc_io_by_pid(pid: u32) -> Option<IO_COUNTERS> {
    let h = ProcHandle::open(pid)?;
    let mut io: IO_COUNTERS = unsafe { std::mem::zeroed() };
    (unsafe { GetProcessIoCounters(h.0, &raw mut io) } != 0).then_some(io)
}

/// (private commit, working set, open handles) for one pid. None if it exited.
fn proc_mem_handles(pid: u32) -> Option<(u64, u64, u32)> {
    let h = ProcHandle::open(pid)?;
    let mut pmc: PROCESS_MEMORY_COUNTERS = unsafe { std::mem::zeroed() };
    pmc.cb = std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32;
    if unsafe { K32GetProcessMemoryInfo(h.0, &raw mut pmc, pmc.cb) } == 0 {
        return None;
    }
    let mut handles: u32 = 0;
    if unsafe { GetProcessHandleCount(h.0, &raw mut handles) } == 0 {
        handles = 0;
    }
    Some((pmc.PagefileUsage as u64, pmc.WorkingSetSize as u64, handles))
}

/// One toolhelp process snapshot → (pid, ppid, threads, name) rows.
fn snapshot_processes() -> Result<Vec<(u32, u32, u32, String)>> {
    let snap = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
    if snap == INVALID_HANDLE_VALUE {
        bail!("CreateToolhelp32Snapshot failed");
    }
    let mut entry: PROCESSENTRY32W = unsafe { std::mem::zeroed() };
    entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
    let mut rows = Vec::new();
    let mut ok = unsafe { Process32FirstW(snap, &raw mut entry) };
    while ok != 0 {
        let len = entry
            .szExeFile
            .iter()
            .position(|&c| c == 0)
            .unwrap_or(entry.szExeFile.len());
        rows.push((
            entry.th32ProcessID,
            entry.th32ParentProcessID,
            entry.cntThreads,
            String::from_utf16_lossy(&entry.szExeFile[..len]),
        ));
        ok = unsafe { Process32NextW(snap, &raw mut entry) };
    }
    unsafe { CloseHandle(snap) };
    Ok(rows)
}

/// The authoritative process membership of the job - every process the kernel says it owns.
/// Unlike a parent-tree walk this cannot miss a reparented process (one whose parent exited
/// and Windows reparented it to the system), which is exactly what a fire-and-forget spawner
/// looks like and what the child-process allowlist exists to catch. Fixed 1024-entry buffer
/// (entries are pointer-sized); a job owning more than that is catastrophic, so this asserts
/// rather than grows.
fn job_process_ids(job: HANDLE) -> Result<Vec<u32>> {
    const CAP: usize = 1024;
    #[repr(C)]
    struct JobPidList {
        header: JOBOBJECT_BASIC_PROCESS_ID_LIST, // {assigned, in_list, ProcessIdList: [usize; 1]}
        rest: [usize; CAP - 1],                  // reserve the rest of the trailing array, contiguous
    }
    let mut buf: JobPidList = unsafe { std::mem::zeroed() };
    let ok = unsafe {
        QueryInformationJobObject(
            job,
            JobObjectBasicProcessIdList,
            (&raw mut buf).cast::<core::ffi::c_void>(),
            std::mem::size_of::<JobPidList>() as u32,
            std::ptr::null_mut(),
        )
    };
    if ok == 0 {
        bail!("QueryInformationJobObject(BasicProcessIdList) failed");
    }
    let assigned = buf.header.NumberOfAssignedProcesses as usize;
    if assigned > CAP {
        bail!("job owns {assigned} processes, over census capacity {CAP} — catastrophic, failing loud");
    }
    let n = buf.header.NumberOfProcessIdsInList as usize;
    // pids are contiguous starting at ProcessIdList[0] (header's last field), running into `rest`.
    let first = buf.header.ProcessIdList.as_ptr();
    Ok((0..n).map(|i| unsafe { *first.add(i) } as u32).collect())
}

/// (total threads over the job's processes, non-root image names, all job pids). Membership is
/// the JOB's authoritative list; per-pid thread count + image name come from a process snapshot
/// (best-effort — a job pid reaped between the two calls is simply skipped, contributing nothing).
fn census(job: HANDLE, root_pid: u32) -> Result<(u32, Vec<String>, Vec<u32>)> {
    let family = job_process_ids(job)?;
    let by_pid: std::collections::HashMap<u32, (u32, String)> = snapshot_processes()?
        .into_iter()
        .map(|r| (r.0, (r.2, r.3)))
        .collect();
    let mut threads = 0u32;
    let mut children = Vec::new();
    for &p in &family {
        if let Some((t, name)) = by_pid.get(&p) {
            threads += t;
            if p != root_pid {
                children.push(name.clone());
            }
        }
    }
    Ok((threads, children, family))
}

/// Resume the single suspended main thread of a freshly created process. A `CREATE_SUSPENDED`
/// process has exactly one thread; find it by owner pid and `ResumeThread` it.
fn resume_process_main_thread(pid: u32) -> Result<()> {
    let snap = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) };
    if snap == INVALID_HANDLE_VALUE {
        bail!("thread snapshot failed");
    }
    let mut te: THREADENTRY32 = unsafe { std::mem::zeroed() };
    te.dwSize = std::mem::size_of::<THREADENTRY32>() as u32;
    let mut tid = None;
    let mut ok = unsafe { Thread32First(snap, &raw mut te) };
    while ok != 0 {
        if te.th32OwnerProcessID == pid {
            tid = Some(te.th32ThreadID);
            break;
        }
        ok = unsafe { Thread32Next(snap, &raw mut te) };
    }
    unsafe { CloseHandle(snap) };
    let tid = tid.context("suspended app has no thread in the snapshot")?;
    let th = unsafe { OpenThread(THREAD_SUSPEND_RESUME, 0, tid) };
    if th.is_null() {
        bail!("OpenThread on the app's main thread failed");
    }
    let prev = unsafe { ResumeThread(th) };
    unsafe { CloseHandle(th) };
    if prev == u32::MAX {
        bail!("ResumeThread failed");
    }
    Ok(())
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    /// Put a cheap child in `job` for the census to find, with no race: the process is created
    /// suspended (so `/c exit` never runs) and assigned to the job before it can touch anything;
    /// it is never resumed, so the census sees a deterministic member. The test's own process
    /// is never put in a job - job assignment is irreversible and process-wide, so it would
    /// constrain (and on close could kill) the whole `cargo test` runner.
    fn suspended_child_in(job: HANDLE) -> std::process::Child {
        let child = std::process::Command::new("cmd.exe")
            .args(["/c", "exit"]) // never executes — the process stays suspended
            .creation_flags(CREATE_SUSPENDED)
            .spawn()
            .expect("spawn suspended cmd.exe");
        // Nested jobs are allowed on Win8+, so this succeeds even if the runner is itself in a job.
        assert_ne!(
            unsafe { AssignProcessToJobObject(job, child_handle(&child)) },
            0,
            "AssignProcessToJobObject failed — is the test runner in a non-nestable job?"
        );
        child
    }

    // Always-on coverage for the census primitives (`job_process_ids` + `census`), otherwise
    // exercised only under the opt-in NEURON_BUDGET_EXE real-binary lane. Two members so the
    // pid-list count/extraction runs past the trivial single-entry case.
    #[test]
    fn census_reports_job_members_by_pid_and_name() {
        let job = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
        assert!(!job.is_null(), "CreateJobObjectW failed");
        let mut kids: Vec<std::process::Child> = (0..2).map(|_| suspended_child_in(job)).collect();

        let pids = job_process_ids(job).expect("job_process_ids");
        for k in &kids {
            assert!(
                pids.contains(&k.id()),
                "job pid list {pids:?} missing member {}",
                k.id()
            );
        }
        assert!(
            pids.len() >= kids.len(),
            "expected at least {} job pids, got {}",
            kids.len(),
            pids.len()
        );

        // Treat one child as the "root": the census must still list every member in `family` but
        // exclude the root from the non-root `children` names — where the OTHER member surfaces as
        // cmd.exe, proving the snapshot join + root filtering.
        let root = kids[0].id();
        let (_threads, children, family) = census(job, root).expect("census");
        for k in &kids {
            assert!(family.contains(&k.id()), "census family {family:?} missing {}", k.id());
        }
        assert!(
            children.iter().any(|n| n.eq_ignore_ascii_case("cmd.exe")),
            "census non-root children {children:?} should name cmd.exe"
        );

        for k in &mut kids {
            let _ = k.kill(); // terminate the suspended members; they were never resumed
        }
        unsafe { CloseHandle(job) };
    }
}
