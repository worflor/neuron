// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

//! FLIGHT RECORDER — Neuron's always-on, crash-surviving diagnostics core.
//!
//! The lifecycle problem this solves: the app can die in ways the Rust panic hook never sees
//! (native fail-fasts in OS DLLs, stack overflows, aborts) — leaving nothing but a Windows
//! Event-Log entry and a mystery. And a worker thread can silently stall, leaving "the weave
//! engine doesn't seem to be working" with no way to tell which organ went quiet.
//!
//! Two primitives, both built on raw atomics — no locks, no allocation on the hot path, safe
//! to read from a crashing process:
//!
//! **The ring** — a fixed static ring of trace events. Each slot has a nonblocking exclusive
//! claim shared by writers and snapshot readers. A collision skips that event or snapshot
//! rather than waiting, so the hot path stays bounded and fields cannot interleave.
//!
//! **The pulses** — named heartbeat atomics that long-lived workers bump every tick. The UI
//! timer reads their ages: a worker silent past its deadline is surfaced as a status warning
//! *while the app is alive* (no more invisible stalls), and every crash dump includes the
//! heartbeat ages — so "what was dead when we died" is part of the record.
//!
//! On any death — Rust panic (hook) or native fault (the SEH unhandled-exception filter in
//! `main`) — [`dump`] appends the whole story to `neuron-crash.log`: uptime, every worker's
//! last heartbeat, and the last ~1k events in order. The crash report stops being a mystery
//! and becomes a narrative.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::OnceLock;
use std::time::Instant;

// ── time base ─────────────────────────────────────────────────────────────────

static START: OnceLock<Instant> = OnceLock::new();

/// Milliseconds since the recorder first ticked (process-start for all practical purposes).
pub fn now_ms() -> u64 {
    START.get_or_init(Instant::now).elapsed().as_millis() as u64
}

/// Milliseconds of AWAKE time since the first call — the heartbeat clock. Windows' `Instant`
/// (QPC) keeps counting through sleep/hibernate, so a worker parked in a wait across a suspend
/// would read as silent for the whole nap; unbiased interrupt time stops while the system sleeps.
/// Other platforms' `Instant` (CLOCK_MONOTONIC, mach_absolute_time) already excludes suspend.
#[cfg(windows)]
fn awake_ms() -> u64 {
    fn unbiased_ms() -> u64 {
        let mut t100ns = 0u64;
        // SAFETY: the out-pointer is a live, aligned u64 for the duration of the call.
        unsafe {
            windows_sys::Win32::System::WindowsProgramming::QueryUnbiasedInterruptTime(&raw mut t100ns)
        };
        t100ns / 10_000
    }
    static BASE: OnceLock<u64> = OnceLock::new();
    let base = *BASE.get_or_init(unbiased_ms);
    unbiased_ms().saturating_sub(base)
}

#[cfg(not(windows))]
fn awake_ms() -> u64 {
    static BASE: OnceLock<Instant> = OnceLock::new();
    BASE.get_or_init(Instant::now).elapsed().as_millis() as u64
}

#[cfg(windows)]
#[inline]
fn tid() -> u32 {
    unsafe { windows_sys::Win32::System::Threading::GetCurrentThreadId() }
}

#[cfg(not(windows))]
#[inline]
fn tid() -> u32 {
    0
}

// ── the ring (per-slot nonblocking exclusive claim) ──────────────────────────

const RING: usize = 1024; // power of two
const MASK: usize = RING - 1;

struct Slot {
    /// Exclusive nonblocking claim for either a writer or a snapshot reader.
    busy: AtomicBool,
    /// Odd while a writer is publishing; 0 = never written.
    seq: AtomicU32,
    t_ms: AtomicU32,
    tid: AtomicU32,
    cat_ptr: AtomicUsize,
    cat_len: AtomicUsize,
    msg_ptr: AtomicUsize,
    msg_len: AtomicUsize,
    arg: AtomicU64,
}

#[allow(clippy::declare_interior_mutable_const)] // the const is the array-init template
const EMPTY: Slot = Slot {
    busy: AtomicBool::new(false),
    seq: AtomicU32::new(0),
    t_ms: AtomicU32::new(0),
    tid: AtomicU32::new(0),
    cat_ptr: AtomicUsize::new(0),
    cat_len: AtomicUsize::new(0),
    msg_ptr: AtomicUsize::new(0),
    msg_len: AtomicUsize::new(0),
    arg: AtomicU64::new(0),
};

static SLOTS: [Slot; RING] = [EMPTY; RING];
static CURSOR: AtomicUsize = AtomicUsize::new(0);

/// Record one event. `cat`/`msg` must be `'static` literals — only the pointer+length are
/// stored, so the hot path never allocates or copies. `arg` carries one numeric detail
/// (a VK, a count, an exchange number — whatever tells the story).
pub fn trace(cat: &'static str, msg: &'static str, arg: u64) {
    let t = now_ms() as u32;
    let i = CURSOR.fetch_add(1, Ordering::Relaxed) & MASK;
    let s = &SLOTS[i];
    if s.busy
        .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
        .is_err()
    {
        return;
    }
    s.seq.store(1, Ordering::Relaxed);
    s.t_ms.store(t, Ordering::Relaxed);
    s.tid.store(tid(), Ordering::Relaxed);
    s.cat_ptr.store(cat.as_ptr() as usize, Ordering::Relaxed);
    s.cat_len.store(cat.len(), Ordering::Relaxed);
    s.msg_ptr.store(msg.as_ptr() as usize, Ordering::Relaxed);
    s.msg_len.store(msg.len(), Ordering::Relaxed);
    s.arg.store(arg, Ordering::Relaxed);
    s.seq.store(2, Ordering::Release);
    s.busy.store(false, Ordering::Release);
}

/// One stable snapshot of a slot, or None if unwritten or currently claimed.
fn read_slot(s: &Slot) -> Option<(u32, u32, &'static str, &'static str, u64)> {
    if s.busy
        .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
        .is_err()
    {
        return None;
    }
    let seq = s.seq.load(Ordering::Relaxed);
    let t = s.t_ms.load(Ordering::Relaxed);
    let id = s.tid.load(Ordering::Relaxed);
    let cp = s.cat_ptr.load(Ordering::Relaxed);
    let cl = s.cat_len.load(Ordering::Relaxed);
    let mp = s.msg_ptr.load(Ordering::Relaxed);
    let ml = s.msg_len.load(Ordering::Relaxed);
    let arg = s.arg.load(Ordering::Relaxed);
    s.busy.store(false, Ordering::Release);
    if seq == 0 || seq & 1 == 1 || cp == 0 || mp == 0 {
        return None;
    }
    // SAFETY: The exclusive claim kept each pointer paired with its length. `trace` accepts only
    // immutable 'static strings, so releasing the slot cannot invalidate their storage or UTF-8.
    let cat =
        unsafe { std::str::from_utf8_unchecked(std::slice::from_raw_parts(cp as *const u8, cl)) };
    let msg =
        unsafe { std::str::from_utf8_unchecked(std::slice::from_raw_parts(mp as *const u8, ml)) };
    Some((t, id, cat, msg, arg))
}

// ── the pulses (worker heartbeats) ───────────────────────────────────────────

/// The long-lived workers the recorder watches. Index = identity (fixed at compile time so
/// the heartbeat store is one relaxed atomic, no registry, no locks).
pub mod organ {
    /// The live dispatch worker (device events → engine).
    pub const DISPATCH: usize = 0;
    /// The weave presenter (spellweaving / beacons / instruments).
    pub const WEAVE: usize = 1;
    /// A live knockback session (cleared on exit — only watched while it plays).
    pub const KNOCKBACK: usize = 2;
    /// The whiteboard session thread (cleared on exit).
    pub const WHITEBOARD: usize = 3;
}

const ORGANS: usize = 4;
const ORGAN_NAMES: [&str; ORGANS] = ["live dispatch", "weave service", "knockback", "whiteboard"];

/// `0` = not running (never started or deliberately cleared); else last-beat `awake_ms`+1.
static PULSES: [AtomicU64; ORGANS] = [
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
];

/// The thread each organ last beat from (0 = never), so a stall probe can find it.
static TIDS: [AtomicU32; ORGANS] = [
    AtomicU32::new(0),
    AtomicU32::new(0),
    AtomicU32::new(0),
    AtomicU32::new(0),
];

/// Beat the heart of `organ::*`. Call from the worker's own loop — two relaxed stores.
pub fn pulse(organ: usize) {
    PULSES[organ].store(awake_ms() + 1, Ordering::Relaxed);
    TIDS[organ].store(tid(), Ordering::Relaxed);
}

/// Mark an organ stopped on purpose (a session ending is not a stall).
pub fn pulse_clear(organ: usize) {
    PULSES[organ].store(0, Ordering::Relaxed);
}

/// How long an organ may go silent before a watcher treats its still-set "running" flag as a
/// crash/stall rather than live work. A session beats every few ms; the click-guard's deadman
/// uses the same order of magnitude (1.5s) to free the mouse when the weave that armed it stops.
pub const SESSION_STALL_MS: u64 = 1500;

/// A gap this long between two reads of the heartbeats means the readers were frozen with
/// everything else (Modern Standby, a suspended VM, a debugger break), since the watcher reads
/// them every 500 ms. That is not a stall of each worker.
const FREEZE_GAP_MS: u64 = 5_000;

/// Last read of the heartbeats, as `awake_ms`+1 (0 = never read).
static LAST_LOOK: AtomicU64 = AtomicU64::new(0);

/// The heartbeat clock for a reader, after crediting a freeze (see [`forgive_freeze`]).
fn look() -> u64 {
    let now = awake_ms();
    forgive_freeze(&PULSES, &LAST_LOOK, now);
    now
}

/// After a gap longer than [`FREEZE_GAP_MS`] since the previous read, credit every running organ
/// with a beat at `now`. A worker that really stopped still surfaces one threshold later.
fn forgive_freeze(pulses: &[AtomicU64], last_look: &AtomicU64, now: u64) {
    let prev = last_look.swap(now + 1, Ordering::Relaxed);
    if prev != 0 && now.saturating_sub(prev - 1) > FREEZE_GAP_MS {
        for p in pulses {
            let _ = p.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |v| (v != 0).then_some(now + 1));
        }
    }
}

/// Milliseconds since `organ` last beat, or `None` if it isn't running (deliberately cleared or
/// never started).
pub fn beat_age(organ: usize) -> Option<u64> {
    let now = look();
    let last = PULSES[organ].load(Ordering::Relaxed);
    (last != 0).then(|| now.saturating_sub(last - 1))
}

/// True if `organ` is RUNNING but has gone silent past [`SESSION_STALL_MS`] — a session whose
/// flag is still set but whose thread has died or wedged. Callers self-heal a key-standdown (or
/// any "is this session really alive?" gate) with this, the way the click-guard frees the mouse.
/// A never-started or deliberately-cleared organ is NOT stalled (it returns `false`), so a session
/// that's just spinning up never has its standdown lifted out from under it.
pub fn organ_stalled(organ: usize) -> bool {
    beat_age(organ).is_some_and(|age| age > SESSION_STALL_MS)
}

/// Uptime in milliseconds since the recorder woke — for the RELIABILITY readout.
pub fn uptime_ms() -> u64 {
    now_ms()
}

/// Snapshot every watched organ for the UI: `(name, beat_age_ms or None if at rest, stalled?)`.
/// A cold path — the reliability panel reads it ~once a second.
pub fn organ_status() -> Vec<(&'static str, Option<u64>, bool)> {
    (0..ORGANS)
        .map(|i| {
            let age = beat_age(i);
            (
                ORGAN_NAMES[i],
                age,
                age.is_some_and(|a| a > SESSION_STALL_MS),
            )
        })
        .collect()
}

/// The crash log lives in the run root (next to the exe) — the panic hook, the SEH filter, the
/// on-demand dump, and the RELIABILITY panel's reader must all mean the SAME file.
pub fn crash_log_path() -> std::path::PathBuf {
    neuron::runroot::run_root().join("neuron-crash.log")
}

/// How many crash/stall dumps `neuron-crash.log` holds (0 if absent/unreadable) — the panel's
/// honest "has this app ever fallen over?" count, read straight from the on-disk record.
pub fn crash_dump_count() -> usize {
    std::fs::read_to_string(crash_log_path())
        .map_or(0, |s| s.matches("dump reason:").count())
}

/// Every running organ whose heart has been silent longer than `max_age_ms`:
/// `(name, silent_for_ms)`. Cold path — the UI timer calls this once a second.
pub fn stalls(max_age_ms: u64) -> Vec<(&'static str, u64)> {
    let now = look();
    let mut out = Vec::new();
    for (i, p) in PULSES.iter().enumerate() {
        let last = p.load(Ordering::Relaxed);
        if last == 0 {
            continue; // not running
        }
        let age = now.saturating_sub(last - 1);
        if age > max_age_ms {
            out.push((ORGAN_NAMES[i], age));
        }
    }
    out
}

// ── the dump (the story, written on any death or on demand) ─────────────────

/// Append the recorder's full state: uptime, heartbeat ages, then the ring oldest→newest.
/// Best-effort by design — called from panic hooks and the SEH filter, where half a story
/// beats no story.
pub fn dump(w: &mut dyn std::io::Write) {
    let now = now_ms();
    let _ = writeln!(
        w,
        "── flight recorder ── uptime {:.1}s",
        now as f64 / 1000.0
    );
    let awake = awake_ms();
    for (i, p) in PULSES.iter().enumerate() {
        let last = p.load(Ordering::Relaxed);
        if last == 0 {
            let _ = writeln!(w, "  organ {:<13} not running", ORGAN_NAMES[i]);
        } else {
            let _ = writeln!(
                w,
                "  organ {:<13} beat {:.1}s ago (awake time)",
                ORGAN_NAMES[i],
                awake.saturating_sub(last - 1) as f64 / 1000.0
            );
        }
    }
    // oldest→newest: start one past the cursor (the next slot to be overwritten is the oldest).
    let cur = CURSOR.load(Ordering::Relaxed);
    for k in 0..RING {
        let s = &SLOTS[(cur + k) & MASK];
        if let Some((t, id, cat, msg, arg)) = read_slot(s) {
            let _ = writeln!(
                w,
                "  t+{:>9.3}s [{:>5}] {:<10} {} ({arg})",
                f64::from(t) / 1000.0,
                id,
                cat,
                msg
            );
        }
    }
    let _ = writeln!(w, "── end flight ──");
}

/// Dump into the crash log next to the exe (the same file the panic hook writes).
pub fn dump_to_crash_log(reason: &str) {
    use std::io::Write as _;
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(crash_log_path())
    {
        let _ = writeln!(f, "[flight] dump reason: {reason}");
        dump(&mut f);
        // The input-latency numbers go in every crash report too: a death during heavy input, or a
        // report of "it got laggy and then it died", is far easier to read when the per-stage
        // distribution up to that moment is part of the same record instead of lost with the process.
        //
        // The ALLOCATION-FREE writer, not the pretty table — this runs from the panic hook and the
        // unhandled-exception filter, where the whole point of this module is that it touches no
        // lock and no allocator (a heap-corruption death would otherwise be finished off by its own
        // crash report). It is also written AFTER the flight dump, so the irreplaceable part is
        // already on disk before this line runs at all.
        neuron::latency::write_crash_summary(&mut f);
    }
}

// ── stall probes (app alive only: these allocate, so never from a crash path) ──

/// One look at a thread through the kernel's process snapshot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ThreadSample {
    /// `KTHREAD_STATE`: 1 ready, 2 running, 5 waiting, …
    state: u32,
    /// `KWAIT_REASON`, meaningful while waiting.
    wait: u32,
    /// Kernel + user CPU time, in 100 ns units.
    cpu: u64,
}

/// CPU times taken by [`probe_stall_threads`], for the follow-up delta.
static PROBE: std::sync::Mutex<Option<(u64, [Option<(u32, u64)>; ORGANS])>> =
    std::sync::Mutex::new(None);

/// How long after a probe its follow-up reports the CPU each thread used.
const PROBE_FOLLOW_MS: u64 = 1_000;

/// Sample `tids` (threads of this process) in one kernel snapshot; `None` for any not found.
#[cfg(windows)]
fn sample_threads(tids: &[u32]) -> Vec<Option<ThreadSample>> {
    use windows_sys::Wdk::System::SystemInformation::{NtQuerySystemInformation, SystemProcessInformation};
    use windows_sys::Win32::System::WindowsProgramming::{SYSTEM_PROCESS_INFORMATION, SYSTEM_THREAD_INFORMATION};
    const STATUS_INFO_LENGTH_MISMATCH: i32 = 0xC000_0004_u32 as i32;
    let mut out = vec![None; tids.len()];
    let mut buf: Vec<u64> = vec![0; 64 * 1024];
    loop {
        let bytes = (buf.len() * 8) as u32;
        let mut needed = 0u32;
        // SAFETY: `buf` is a live, 8-aligned buffer of `bytes` bytes for the duration of the call.
        let status = unsafe {
            NtQuerySystemInformation(SystemProcessInformation, buf.as_mut_ptr().cast(), bytes, &raw mut needed)
        };
        if status == STATUS_INFO_LENGTH_MISMATCH && buf.len() < 16 * 1024 * 1024 {
            buf = vec![0; (needed as usize / 8 + 8 * 1024).max(buf.len() * 2)];
            continue;
        }
        if status < 0 {
            return out;
        }
        break;
    }
    let me = std::process::id() as usize;
    let base = buf.as_ptr().cast::<u8>();
    let end = buf.len() * 8;
    let mut offset = 0usize;
    loop {
        if offset + std::mem::size_of::<SYSTEM_PROCESS_INFORMATION>() > end {
            return out;
        }
        // SAFETY: the kernel filled `buf` with a chain of SYSTEM_PROCESS_INFORMATION records, each
        // followed by its NumberOfThreads SYSTEM_THREAD_INFORMATION entries; the bounds check above
        // keeps the read inside `buf`, and `read_unaligned` copies the plain-data record.
        let proc_info: SYSTEM_PROCESS_INFORMATION =
            unsafe { std::ptr::read_unaligned(base.add(offset).cast()) };
        if proc_info.UniqueProcessId as usize == me {
            let threads_at = offset + std::mem::size_of::<SYSTEM_PROCESS_INFORMATION>();
            let count = proc_info.NumberOfThreads as usize;
            if threads_at + count * std::mem::size_of::<SYSTEM_THREAD_INFORMATION>() > end {
                return out;
            }
            for i in 0..count {
                let at = threads_at + i * std::mem::size_of::<SYSTEM_THREAD_INFORMATION>();
                // SAFETY: bounds checked above; the entries follow the process record.
                let t: SYSTEM_THREAD_INFORMATION =
                    unsafe { std::ptr::read_unaligned(base.add(at).cast()) };
                let id = t.ClientId.UniqueThread as usize as u32;
                if let Some(slot) = tids.iter().position(|&want| want != 0 && want == id) {
                    // Reserved1 is [KernelTime, UserTime, CreateTime].
                    let cpu = (t.Reserved1[0].max(0) + t.Reserved1[1].max(0)) as u64;
                    out[slot] = Some(ThreadSample { state: t.ThreadState, wait: t.WaitReason, cpu });
                }
            }
            return out;
        }
        if proc_info.NextEntryOffset == 0 {
            return out;
        }
        offset += proc_info.NextEntryOffset as usize;
    }
}

#[cfg(not(windows))]
fn sample_threads(tids: &[u32]) -> Vec<Option<ThreadSample>> {
    vec![None; tids.len()]
}

fn state_name(s: u32) -> &'static str {
    match s {
        0 => "initialized",
        1 => "ready",
        2 => "running",
        3 => "standby",
        4 => "terminated",
        5 => "waiting",
        6 => "transition",
        7 => "deferred-ready",
        _ => "state?",
    }
}

/// The wait reasons a stalled user-mode worker can plausibly be in; others print as a number.
fn wait_name(w: u32) -> &'static str {
    match w {
        0 | 7 => "executive (kernel object or I/O)",
        2 | 9 => "page-in",
        4 | 11 => "sleep",
        5 | 12 => "suspended",
        6 => "user wait (handle)",
        13 => "user wait (window messages)",
        15 => "queue",
        16 | 17 => "lpc/alpc",
        27 => "resource",
        28 => "push lock",
        29 | 34 | 35 => "mutex",
        37 => "alert by thread id (Rust lock or condvar)",
        _ => "",
    }
}

fn write_sample(w: &mut dyn std::io::Write, name: &str, tid: u32, s: Option<ThreadSample>) {
    match s {
        None => {
            let _ = writeln!(w, "  thread {name:<13} tid {tid} not found");
        }
        Some(s) if s.state == 5 => {
            let _ = writeln!(
                w,
                "  thread {name:<13} tid {tid} waiting, reason {} {}, cpu {:.2}s",
                s.wait,
                wait_name(s.wait),
                s.cpu as f64 / 1e7
            );
        }
        Some(s) => {
            let _ = writeln!(
                w,
                "  thread {name:<13} tid {tid} {}, cpu {:.2}s",
                state_name(s.state),
                s.cpu as f64 / 1e7
            );
        }
    }
}

/// The running organs' (index, thread id) pairs.
fn running_organ_tids() -> Vec<(usize, u32)> {
    (0..ORGANS)
        .filter(|&i| PULSES[i].load(Ordering::Relaxed) != 0)
        .map(|i| (i, TIDS[i].load(Ordering::Relaxed)))
        .filter(|&(_, tid)| tid != 0)
        .collect()
}

fn append_to_crash_log(write: impl FnOnce(&mut dyn std::io::Write)) {
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(crash_log_path()) {
        write(&mut f);
    }
}

/// After a stall dump: append each running organ's thread state, wait reason and CPU time to the
/// crash log, and arm [`follow_stall_probe`]. A stalled thread that is waiting with flat CPU is
/// stuck; one that is running or ready is busy or starved.
pub fn probe_stall_threads() {
    let organs = running_organ_tids();
    let tids: Vec<u32> = organs.iter().map(|&(_, t)| t).collect();
    let samples = sample_threads(&tids);
    let mut cpu = [None; ORGANS];
    append_to_crash_log(|w| {
        let _ = writeln!(w, "── stall threads ──");
        for (&(organ, tid), s) in organs.iter().zip(&samples) {
            write_sample(w, ORGAN_NAMES[organ], tid, *s);
            cpu[organ] = s.map(|s| (tid, s.cpu));
        }
    });
    *PROBE.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = Some((awake_ms(), cpu));
}

/// Call from the watcher tick. Once, [`PROBE_FOLLOW_MS`] after a probe, append how much CPU each
/// probed thread used since and its state now.
pub fn follow_stall_probe() {
    let probe = {
        let mut g = PROBE.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        match *g {
            Some((at, _)) if awake_ms().saturating_sub(at) >= PROBE_FOLLOW_MS => g.take(),
            _ => None,
        }
    };
    let Some((at, before)) = probe else {
        return;
    };
    let probed: Vec<(usize, u32, u64)> = before
        .iter()
        .enumerate()
        .filter_map(|(i, b)| b.map(|(tid, cpu)| (i, tid, cpu)))
        .collect();
    let tids: Vec<u32> = probed.iter().map(|&(_, t, _)| t).collect();
    let samples = sample_threads(&tids);
    let elapsed = awake_ms().saturating_sub(at);
    append_to_crash_log(|w| {
        let _ = writeln!(w, "── stall threads, {:.1}s later ──", elapsed as f64 / 1000.0);
        for (&(organ, tid, cpu), s) in probed.iter().zip(&samples) {
            let used = s.map(|s| s.cpu.saturating_sub(cpu) / 10_000);
            write_sample(w, ORGAN_NAMES[organ], tid, *s);
            if let Some(ms) = used {
                let _ = writeln!(w, "    used {ms} ms of cpu in that time");
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex, OnceLock};

    fn test_guard() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(())).lock().unwrap()
    }

    #[test]
    fn trace_then_dump_tells_the_story() {
        let _guard = test_guard();
        trace("test", "alpha", 1);
        trace("test", "beta", 2);
        let mut buf = Vec::new();
        dump(&mut buf);
        let s = String::from_utf8(buf).unwrap();
        assert!(s.contains("alpha (1)"), "dump should carry the event: {s}");
        assert!(s.contains("beta (2)"));
        assert!(s.contains("uptime"));
    }

    #[test]
    fn concurrent_writers_never_mix_slot_fields() {
        let _guard = test_guard();
        let categories = ["pair0", "pair1", "pair2", "pair3"];
        let messages = ["message0", "message1", "message2", "message3"];
        let threads: Vec<_> = (0..4)
            .map(|n| {
                let cat = categories[n];
                let msg = messages[n];
                std::thread::spawn(move || {
                    for i in 0..20_000u64 {
                        trace(cat, msg, n as u64 * 1_000_000 + i);
                    }
                })
            })
            .collect();
        let check_snapshot = |bytes: &[u8]| {
            let text = String::from_utf8_lossy(bytes);
            for line in text.lines().filter(|line| line.contains("message")) {
                let matched = (0..4).find(|&n| line.contains(messages[n]));
                let Some(n) = matched else {
                    panic!("unexpected event message: {line}");
                };
                assert!(line.contains(categories[n]), "mixed category/message: {line}");
                let arg = line
                    .rsplit_once('(')
                    .and_then(|(_, rest)| rest.strip_suffix(')'))
                    .and_then(|value| value.parse::<u64>().ok())
                    .expect("event argument is present");
                assert_eq!(arg / 1_000_000, n as u64, "mixed event argument: {line}");
            }
            text.contains("message")
        };
        let mut saw_event = false;
        for _ in 0..200 {
            let mut snapshot = Vec::new();
            dump(&mut snapshot);
            saw_event |= check_snapshot(&snapshot);
        }
        for t in threads {
            t.join().unwrap();
        }
        let mut buf = Vec::new();
        dump(&mut buf);
        saw_event |= check_snapshot(&buf);
        assert!(saw_event, "events survive the stampede");
    }

    #[test]
    fn heartbeat_clock_tracks_awake_time() {
        let a = awake_ms();
        std::thread::sleep(std::time::Duration::from_millis(120));
        let b = awake_ms();
        // Unbiased interrupt time ticks at the system timer (~15.6 ms), so allow one tick of slop.
        assert!(b >= a + 100, "awake clock advances while awake: {a} -> {b}");
        assert!(b <= a + 2_000, "awake clock does not run away: {a} -> {b}");
    }

    /// The probe sees a parked thread as waiting with flat CPU and a spinning one burning CPU.
    #[cfg(windows)]
    #[test]
    fn stall_probe_tells_a_waiting_thread_from_a_busy_one() {
        use std::sync::atomic::AtomicBool;
        use std::sync::mpsc::channel;
        use std::sync::Arc;
        let (tids, tid_rx) = channel::<(bool, u32)>();
        let (release, parked) = channel::<()>();
        let stop = Arc::new(AtomicBool::new(false));
        let waiter = {
            let tids = tids.clone();
            std::thread::spawn(move || {
                tids.send((false, tid())).unwrap();
                let _ = parked.recv();
            })
        };
        let spinner = {
            let stop = stop.clone();
            std::thread::spawn(move || {
                tids.send((true, tid())).unwrap();
                while !stop.load(Ordering::Relaxed) {
                    std::hint::spin_loop();
                }
            })
        };
        let (mut wait_tid, mut spin_tid) = (0, 0);
        for _ in 0..2 {
            let (busy, t) = tid_rx.recv().unwrap();
            if busy { spin_tid = t } else { wait_tid = t }
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
        let before = sample_threads(&[wait_tid, spin_tid, u32::MAX]);
        std::thread::sleep(std::time::Duration::from_millis(300));
        let after = sample_threads(&[wait_tid, spin_tid]);
        stop.store(true, Ordering::Relaxed);
        release.send(()).unwrap();
        waiter.join().unwrap();
        spinner.join().unwrap();

        assert_eq!(before[2], None, "a thread id that is not ours is not found");
        let (w0, w1) = (before[0].unwrap(), after[0].unwrap());
        let (s0, s1) = (before[1].unwrap(), after[1].unwrap());
        assert_eq!(w1.state, 5, "a parked thread reads as waiting: {w1:?}");
        assert_ne!(s1.state, 5, "a spinning thread is running or ready: {s1:?}");
        assert!(s1.cpu > s0.cpu, "the spinner used cpu: {s0:?} -> {s1:?}");
        assert!(w1.cpu - w0.cpu < s1.cpu - s0.cpu, "the waiter used less cpu than the spinner");
    }

    /// A long gap between reads credits running organs instead of reporting them all stalled;
    /// a normal cadence changes nothing and a stopped organ stays stopped.
    #[test]
    fn a_frozen_reader_credits_running_organs() {
        let pulses = [AtomicU64::new(0), AtomicU64::new(1_001)];
        let look = AtomicU64::new(0);
        forgive_freeze(&pulses, &look, 1_000);
        forgive_freeze(&pulses, &look, 1_500);
        forgive_freeze(&pulses, &look, 1_500 + FREEZE_GAP_MS);
        assert_eq!(pulses[1].load(Ordering::Relaxed), 1_001, "the watcher's cadence is not a freeze");
        forgive_freeze(&pulses, &look, 90_000);
        assert_eq!(pulses[1].load(Ordering::Relaxed), 90_001, "running organ credited after a freeze");
        assert_eq!(pulses[0].load(Ordering::Relaxed), 0, "a stopped organ stays stopped");
        forgive_freeze(&pulses, &look, 90_400);
        assert_eq!(pulses[1].load(Ordering::Relaxed), 90_001, "credit is once per freeze");
    }

    #[test]
    fn pulse_and_stall_detection() {
        let _guard = test_guard();
        pulse(organ::WHITEBOARD);
        assert!(
            stalls(60_000).iter().all(|(n, _)| *n != "whiteboard"),
            "a fresh beat is not a stall"
        );
        // let the heart actually go silent past a tiny deadline
        std::thread::sleep(std::time::Duration::from_millis(40));
        let st = stalls(10);
        assert!(
            st.iter().any(|(n, _)| *n == "whiteboard"),
            "an old beat is a stall: {st:?}"
        );
        // a new beat heals it; a clear retires it
        pulse(organ::WHITEBOARD);
        assert!(
            stalls(10_000).iter().all(|(n, _)| *n != "whiteboard"),
            "a beat heals"
        );
        pulse_clear(organ::WHITEBOARD);
        assert!(
            stalls(0).iter().all(|(n, _)| *n != "whiteboard"),
            "a cleared organ is at rest, not stalled"
        );
    }
}
