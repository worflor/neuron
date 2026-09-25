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

/// `0` = not running (never started or deliberately cleared); else last-beat `now_ms`+1.
static PULSES: [AtomicU64; ORGANS] = [
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
];

/// Beat the heart of `organ::*`. Call from the worker's own loop — one relaxed store.
pub fn pulse(organ: usize) {
    PULSES[organ].store(now_ms() + 1, Ordering::Relaxed);
}

/// Mark an organ stopped on purpose (a session ending is not a stall).
pub fn pulse_clear(organ: usize) {
    PULSES[organ].store(0, Ordering::Relaxed);
}

/// How long an organ may go silent before a watcher treats its still-set "running" flag as a
/// crash/stall rather than live work. A session beats every few ms; the click-guard's deadman
/// uses the same order of magnitude (1.5s) to free the mouse when the weave that armed it stops.
pub const SESSION_STALL_MS: u64 = 1500;

/// Milliseconds since `organ` last beat, or `None` if it isn't running (deliberately cleared or
/// never started).
pub fn beat_age(organ: usize) -> Option<u64> {
    let last = PULSES[organ].load(Ordering::Relaxed);
    (last != 0).then(|| now_ms().saturating_sub(last - 1))
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
    let now = now_ms();
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
    for (i, p) in PULSES.iter().enumerate() {
        let last = p.load(Ordering::Relaxed);
        if last == 0 {
            let _ = writeln!(w, "  organ {:<13} not running", ORGAN_NAMES[i]);
        } else {
            let _ = writeln!(
                w,
                "  organ {:<13} beat {:.1}s ago",
                ORGAN_NAMES[i],
                now.saturating_sub(last - 1) as f64 / 1000.0
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
