// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

//! the Macro Host — Neuron's macro runtime. A bundled private CPython, run as ONE warm sidecar
//! process. Macros are real Python (`import ctypes`/`subprocess`/anything — full unsandboxed
//! power, "as if it were a program"); they're registered once (imports warmed) and a trigger is a
//! tiny framed message that calls the already-resident function. No per-press spawn, no per-press
//! import — real-time for triggered macros. The native [`crate::action`] engine still owns
//! per-frame key→key remaps at literal 0ns; the Macro Host never touches the 1000 Hz input path.
//!
//! ## Why a sidecar (not in-process)
//! A macro doing raw `ctypes` is one bad pointer from a segfault. In-process that would take down
//! the app that controls the user's hardware. The sidecar is FIREWALLED: a crashing/hanging macro
//! kills only the sidecar, which the host respawns + re-registers in the background while the main
//! app never hitches. This is strictly safer than the cdylib tower it replaces (which could only
//! *detach-and-leak* a runaway thread inside the app).
//!
//! ## Transport (the load-bearing isolation)
//! Three standard pipes, the protocol NEVER on a stream a macro can reach:
//!   * host→sidecar control  = child STDIN  (framed JSON)
//!   * sidecar→host protocol = child STDOUT — but the host script dups its real stdout aside as the
//!     private protocol channel BEFORE any macro runs, then redirects fd 1→2, so a macro's
//!     `print()`/`os.write(1)` physically cannot corrupt a frame (proven in the host self-test).
//!   * macro log             = child STDERR (captured to a bounded ring; surfaced, never parsed)
//!
//! Frame = `[u32 LE length][utf-8 JSON]`.
//!
//! Nothing here ever blocks the input/UI thread: [`fire_async`] dispatches and returns; the
//! blocking [`invoke`]/[`check`] are for the GUI "test run" + CLI only, always with a hard budget.

use crate::macros::node::{MacroDocument, MacroNode};
use crate::macros::policy::{mode_from_source, MacroMode};
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::io::{BufReader, Read, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Condvar, Mutex, OnceLock, Weak};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

#[cfg(windows)]
use std::os::windows::process::CommandExt;

// DETACHED_PROCESS, not CREATE_NO_WINDOW: NO_WINDOW still allocates a (hidden) console, so a
// conhost.exe rides along with every sidecar — one more background process on the user's box
// (caught live by the testkit budget lane's child census, 2026-07-10). The sidecar speaks only
// over its three stdio PIPES, which need no console; DETACHED gives it none and no conhost.
#[cfg(windows)]
const DETACHED_PROCESS: u32 = 0x0000_0008;

/// The macro-log ring, shared across sessions so a crashed sidecar's last stderr (its traceback)
/// survives the respawn that replaces the session. (A per-session ring would be dropped with the
/// dead session before the GUI could read it.)
type LogRing = Arc<Mutex<VecDeque<String>>>;

/// The beacon channel slot — where a UI (the GUI's beacon service, the CLI's terminal prompt)
/// installs its [`Sender`]. MacroHost-level like the log ring, so it survives sidecar respawns.
/// `None` (or a dropped receiver) means "no UI attached": prompts are auto-dismissed honestly.
type BeaconSlot = Arc<Mutex<Option<Sender<BeaconEvent>>>>;

/// An event from a macro's BEACON layer — the two-part "prime then activate" surface. A running
/// macro calls `neuron.ask("…")` and blocks (its own worker thread only) until the user answers
/// through the binary radial (or the prompt times out / is dismissed). The host routes these to
/// whatever UI installed itself via [`MacroHost::beacon_events`].
#[derive(Debug, Clone)]
pub enum BeaconEvent {
    /// A macro is prompting the user to pick one of `options` — the ANSWER WHEEL. N=2 is yes/no, N≥3
    /// a radial menu, N=1 a confirm; all one path. Answer with [`MacroHost::answer`] — `Some(i)` =
    /// option `i`, `None` = passed/dismissed (the macro's prompt returns its `default`).
    Ask {
        /// The prompt id to answer with (sidecar-allocated; unique per ask).
        pid: u64,
        /// The macro that asked (for the readout).
        macro_id: String,
        /// The question, verbatim.
        text: String,
        /// The ordered option labels — the wheel's wedges (`["yes","no"]` for a plain `ask`).
        options: Vec<String>,
        /// Optional context shown UNDER the answer wheel (`description=`); "" = none.
        detail: String,
        /// The sidecar-side timeout, if the macro set one — the UI should retire the prompt itself
        /// at this deadline (the sidecar already has; an answer after it is ignored).
        timeout_ms: Option<u64>,
    },
    /// A prompt resolved without the UI (sidecar-side timeout) — withdraw it from display.
    Retire { pid: u64 },
    /// Every open prompt is void (the sidecar died/respawned) — clear the queue.
    RetireAll,
    /// A macro's fire-and-forget status line (`neuron.notify("…")`) — show it, don't block.
    Notify { macro_id: String, text: String },
}

/// Why a [`MacroHost::parse_macro`] could not produce a node tree. A `SyntaxError` in the source is
/// the EXPECTED case (a half-typed macro) and carries the offending line + message verbatim from
/// Python's `ast`; everything else (no python runtime, sidecar pipe broken, timeout) is a `Host`
/// error. The constructor UI surfaces `Syntax` inline at the line and treats `Host` as "try again".
#[derive(Clone, Debug, PartialEq)]
pub enum ParseError {
    /// The source didn't parse. `line` is the 1-based line (0 if Python gave none); `msg` is
    /// Python's syntax-error message.
    Syntax { line: u32, msg: String },
    /// The source's leading Neuron compiler directive is malformed or contradictory.
    Policy { line: u32, msg: String },
    /// The sidecar/runtime couldn't service the request (no python, pipe broken, did not answer).
    Host { msg: String },
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ParseError::Syntax { line, msg } => write!(f, "syntax error (line {line}): {msg}"),
            ParseError::Policy { line, msg } => write!(f, "macro policy error (line {line}): {msg}"),
            ParseError::Host { msg } => write!(f, "{msg}"),
        }
    }
}

impl std::error::Error for ParseError {}

/// The outcome of parsing a macro's Python source into the typed node tree: the macro body as an
/// ordered `Vec<MacroNode>` on success, or a [`ParseError`].
pub type ParseResult = Result<Vec<MacroNode>, ParseError>;
/// Whole-module parse for the visual constructor: preserved module/entry source + typed body.
pub type DocumentParseResult = Result<MacroDocument, ParseError>;

/// Hard ceiling on a single blocking macro invocation (test-run / CLI). The input path uses
/// [`fire_async`] and never waits at all.
pub const FIRE_BUDGET: Duration = Duration::from_millis(2500);
/// How long to wait for the sidecar to come warm (boot + register every macro) before giving up.
const WARM_TIMEOUT: Duration = Duration::from_secs(20);
/// Bounded macro-log ring (last N lines kept; old dropped — a flood can't grow memory).
const LOG_RING: usize = 400;
/// Circuit breaker: more than this many sidecar crashes inside the window trips a cooldown.
const BREAKER_MAX: u32 = 4;
const BREAKER_WINDOW: Duration = Duration::from_secs(30);
const BREAKER_COOLDOWN: Duration = Duration::from_secs(20);

/// The process-global MacroHost. Lazily created (does NOT spawn the sidecar until first use or an
/// explicit [`MacroHost::ensure_warm`] at app launch).
static MACRO_HOST: OnceLock<MacroHost> = OnceLock::new();

/// Reach the process-global MacroHost.
pub fn macro_host() -> &'static MacroHost {
    MACRO_HOST.get_or_init(MacroHost::new)
}

/// State shared with the reader thread: pending request waiters, warm/dead signalling, the log ring.
struct Shared {
    /// rid -> a one-shot waiter for the matching reply (result/checked/pong/registered).
    pending: Mutex<HashMap<u64, Sender<Value>>>,
    /// (warm, dead) + a condvar so `ensure` can wait for the `ready` frame.
    state: Mutex<LinkState>,
    cv: Condvar,
    /// bounded ring of the sidecar's stderr (macro print()/tracebacks). SHARED across sessions
    /// (MacroHost-level) so a crash's final lines aren't lost when the dead session is dropped.
    log: LogRing,
    next_rid: AtomicU64,
}

#[derive(Default)]
struct LinkState {
    warm: bool,
    dead: bool,
}

impl Shared {
    fn push_log(&self, line: String) {
        let mut l = self.log.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if l.len() >= LOG_RING {
            l.pop_front();
        }
        l.push_back(line);
    }
    fn mark_dead(&self) {
        // Race window (a): freezing HERE (before the clear) while a concurrent request's insert
        // proceeds first lets that fresh, legitimate waiter get wiped by the clear below the moment
        // this thread resumes — "mark_dead racing register", the opposite-side sibling of the
        // pending_insert.before window in the request paths (see `macro_host_death_race_*` tests).
        crate::failpoint!("macro_host.mark_dead.before_clear");
        {
            let mut s = self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            s.dead = true;
            s.warm = false;
        }
        self.cv.notify_all();
        // fail every in-flight waiter so no blocking caller hangs past the sidecar's death.
        self.pending.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clear();
        // Race window (b): freezing HERE (after the clear) while a concurrent request's insert has
        // NOT happened yet lets that request insert its waiter into an already-cleared map — and
        // this reader thread is about to exit for good, so nothing will ever route that waiter's
        // reply. Bounded only by the request's own recv_timeout (see FIRE_BUDGET/WARM_TIMEOUT).
        crate::failpoint!("macro_host.mark_dead.after_clear");
    }
}

/// One live sidecar process + its writer end and the threads draining it. `stdin` is shared
/// (Arc<Mutex>) because TWO writers exist: the host's request paths (fire/register/…) and the
/// reader thread's no-UI auto-answer for prompts. stdin is a LEAF lock — never held while taking
/// another — so the two writers can't deadlock.
struct Session {
    child: Child,
    stdin: Arc<Mutex<ChildStdin>>,
    shared: Arc<Shared>,
    reader: Option<JoinHandle<()>>,
    logger: Option<JoinHandle<()>>,
}

/// Frame one JSON request onto a sidecar pipe. Returns false if the pipe is broken (sidecar gone).
fn send_frame(w: &mut impl Write, v: &Value) -> bool {
    let body = match serde_json::to_vec(v) {
        Ok(b) => b,
        Err(_) => return false,
    };
    let len = (body.len() as u32).to_le_bytes();
    w.write_all(&len).is_ok() && w.write_all(&body).is_ok() && w.flush().is_ok()
}

impl Session {
    /// Frame one request to the sidecar. Returns false if the pipe is broken (sidecar gone).
    fn send(&mut self, v: &Value) -> bool {
        send_frame(&mut *self.stdin.lock().unwrap_or_else(std::sync::PoisonError::into_inner), v)
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        // best-effort graceful shutdown, then make sure the process is gone (no orphan sidecars).
        let _ = self.send(&json!({"t": "shutdown"}));
        // bounded wait for a clean exit, then hard-kill — never the blind fixed sleep.
        let deadline = Instant::now() + Duration::from_millis(150);
        loop {
            match self.child.try_wait() {
                Ok(Some(_)) => break,
                _ if Instant::now() >= deadline => {
                    let _ = self.child.kill();
                    let _ = self.child.wait();
                    break;
                }
                _ => std::thread::sleep(Duration::from_millis(5)),
            }
        }
        // The child's pipe write-ends are now closed, so the reader/logger threads hit EOF and
        // exit; join them so teardown is deterministic (no thread/handle pile-up across respawns).
        if let Some(h) = self.reader.take() {
            let _ = h.join();
        }
        if let Some(h) = self.logger.take() {
            let _ = h.join();
        }
    }
}

/// Crash bookkeeping so a macro that segfaults on every press can't pin a core respawning forever.
#[derive(Default)]
struct Breaker {
    crashes: VecDeque<Instant>,
    cooldown_until: Option<Instant>,
}

impl Breaker {
    fn record_crash(&mut self) {
        let now = Instant::now();
        self.crashes.push_back(now);
        while self
            .crashes
            .front()
            .is_some_and(|t| now.duration_since(*t) > BREAKER_WINDOW)
        {
            self.crashes.pop_front();
        }
        if self.crashes.len() as u32 > BREAKER_MAX {
            self.cooldown_until = Some(now + BREAKER_COOLDOWN);
        }
    }
    /// Is the breaker tripped right now (refuse to respawn)?
    fn tripped(&mut self) -> bool {
        match self.cooldown_until {
            Some(t) if Instant::now() < t => true,
            Some(_) => {
                self.cooldown_until = None;
                self.crashes.clear();
                false
            }
            None => false,
        }
    }
    fn reset(&mut self) {
        self.crashes.clear();
        self.cooldown_until = None;
    }
}

struct Inner {
    /// RAW is also the compiler/control lane: parse/check never execute user source.
    session: Option<Session>,
    /// BOUND never cohabits an interpreter with unrestricted Python.
    bound_session: Option<Session>,
    /// id -> python source. The single source of truth for re-registration after a respawn.
    manifest: BTreeMap<String, String>,
    /// id -> source-owned execution domain.
    modes: BTreeMap<String, MacroMode>,
    /// id -> active source generation. Fires carry this number so queued work can never cross a
    /// hot-reload boundary and accidentally execute a newer callable than the trigger selected.
    generations: BTreeMap<String, u64>,
    /// id -> the macro's DECLARED option manifest (its `NEURON_OPTIONS`, re-derived on register).
    /// The GUI renders controls from this; the user's chosen VALUES live on disk (`options_path`).
    options: BTreeMap<String, Value>,
    breaker: Breaker,
    bound_breaker: Breaker,
}

fn lane_session(g: &Inner, mode: MacroMode) -> Option<&Session> {
    match mode {
        MacroMode::Raw => g.session.as_ref(),
        MacroMode::Bound => g.bound_session.as_ref(),
    }
}

fn lane_session_mut(g: &mut Inner, mode: MacroMode) -> Option<&mut Session> {
    match mode {
        MacroMode::Raw => g.session.as_mut(),
        MacroMode::Bound => g.bound_session.as_mut(),
    }
}

fn lane_take(g: &mut Inner, mode: MacroMode) -> Option<Session> {
    match mode {
        MacroMode::Raw => g.session.take(),
        MacroMode::Bound => g.bound_session.take(),
    }
}

fn lane_set(g: &mut Inner, mode: MacroMode, session: Session) {
    match mode {
        MacroMode::Raw => g.session = Some(session),
        MacroMode::Bound => g.bound_session = Some(session),
    }
}

fn lane_breaker_mut(g: &mut Inner, mode: MacroMode) -> &mut Breaker {
    match mode {
        MacroMode::Raw => &mut g.breaker,
        MacroMode::Bound => &mut g.bound_breaker,
    }
}

fn lane_for_shared(g: &Inner, shared: &Arc<Shared>) -> Option<MacroMode> {
    if g.session
        .as_ref()
        .is_some_and(|s| Arc::ptr_eq(&s.shared, shared))
    {
        Some(MacroMode::Raw)
    } else if g
        .bound_session
        .as_ref()
        .is_some_and(|s| Arc::ptr_eq(&s.shared, shared))
    {
        Some(MacroMode::Bound)
    } else {
        None
    }
}

/// The process-global macro runtime.
pub struct MacroHost {
    inner: Mutex<Inner>,
    /// Serialize durable macro mutations. Save/delete are cold control-plane operations; one lock
    /// makes their disk + manifest + sidecar publication order atomic from every caller's view.
    mutations: Mutex<()>,
    /// Monotonic macro revision source. Zero is reserved for "not registered".
    next_generation: AtomicU64,
    /// The SAFE/arm gate, kept OUTSIDE the inner lock so `set_armed` (called on the UI thread) is
    /// always lock-free and instant — it can never stall behind a cold-spawn warm-wait. The spawn
    /// reads this for the initial + warm-handshake arm state, so a respawn always reflects current.
    armed: AtomicBool,
    /// At most one background warm thread per execution domain.
    warm_in_flight: AtomicBool,
    bound_warm_in_flight: AtomicBool,
    /// The macro-log ring, owned here so it survives session respawns (each Session's `Shared.log`
    /// is a clone of this Arc).
    log: LogRing,
    /// Where a UI installs its beacon receiver (see [`MacroHost::beacon_events`]).
    /// MacroHost-level so it survives respawns, like the log ring.
    beacon: BeaconSlot,
}

impl MacroHost {
    fn new() -> Self {
        MacroHost {
            inner: Mutex::new(Inner {
                session: None,
                bound_session: None,
                manifest: BTreeMap::new(),
                modes: BTreeMap::new(),
                generations: BTreeMap::new(),
                options: BTreeMap::new(),
                breaker: Breaker::default(),
                bound_breaker: Breaker::default(),
            }),
            mutations: Mutex::new(()),
            next_generation: AtomicU64::new(1),
            armed: AtomicBool::new(false),
            warm_in_flight: AtomicBool::new(false),
            bound_warm_in_flight: AtomicBool::new(false),
            log: Arc::new(Mutex::new(VecDeque::new())),
            beacon: Arc::new(Mutex::new(None)),
        }
    }

    /// Install THE beacon listener and get its event stream. One listener at a time (a new call
    /// replaces the old — its receiver starts erroring and prompts fall back to auto-dismiss).
    /// With no listener installed, a macro's `ask` is auto-dismissed (returns its `default`) and
    /// the fact is logged — a beacon never strands a macro just because no UI is watching.
    pub fn beacon_events(&self) -> Receiver<BeaconEvent> {
        let (tx, rx) = channel();
        *self.beacon.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = Some(tx);
        rx
    }

    /// Answer an open prompt: `Some(i)` = the user picked option `i`, `None` = passed/dismissed (the
    /// macro's prompt returns its `default`). An unknown/expired pid is ignored by the sidecar —
    /// answering late is always safe.
    pub fn answer(&self, pid: u64, choice: Option<usize>) {
        let mut g = self.inner.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        for mode in [MacroMode::Raw, MacroMode::Bound] {
            if let Some(s) = lane_session_mut(&mut g, mode) {
                let _ = s.send(&json!({"t": "answer", "pid": pid, "choice": choice}));
            }
        }
    }

    /// Mirror the live arm/SAFE state into the sidecar so the helper input layer honours it.
    /// LOCK-FREE: the authoritative value is the atomic (read by the spawn + warm handshake), so a
    /// SAFE toggle on the UI thread is instant and can never stall behind a cold-spawn warm-wait.
    /// A frame is sent best-effort only if the lock is free; otherwise the next warm picks up the
    /// current atomic. (Raw `ctypes` past the helpers is the user's own rope and is not gated.)
    pub fn set_armed(&self, on: bool) {
        self.armed.store(on, Ordering::SeqCst);
        if let Ok(mut g) = self.inner.try_lock() {
            for mode in [MacroMode::Raw, MacroMode::Bound] {
                if let Some(s) = lane_session_mut(&mut g, mode) {
                    let _ = s.send(&json!({"t": "armed", "on": on}));
                }
            }
        }
    }

    /// Whether the BUNDLED Python runtime + host scripts can be materialized (else the macro tier
    /// is disabled and reports it honestly; the rest of Neuron works fully without Python). Since
    /// the interpreter is shipped in the binary, this only fails on a real IO problem (no writable
    /// data dir / extraction error), not on a missing system Python.
    pub fn available(&self) -> bool {
        resolve_runtime().is_ok()
    }

    /// Register (or replace) a macro and publish the new revision only after it is durable.
    ///
    /// Python PREPARES the namespace first but does not expose it to fires. The source is then
    /// atomically written to disk, then the prepared callable is committed. Only after the commit
    /// acknowledgement does dispatch see the new manifest and generation. A failed final commit
    /// restores the previous source and retires the uncertain sidecar session.
    pub fn register(&self, id: &str, source: &str) -> Result<(), String> {
        validate_macro_id(id)?;
        let mode = mode_from_source(source)?;
        let _mutation = self
            .mutations
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let previous_source = load_macro(id);
        let generation = self.next_generation.fetch_add(1, Ordering::Relaxed);
        let old_mode = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .modes
            .get(id)
            .copied();
        let (token, opts, shared) = self.prepare_register(id, source, generation, mode)?;

        if let Err(e) = write_macro_file(id, source) {
            self.discard_prepared(&shared, token);
            return Err(format!("persist macro '{id}': {e}"));
        }

        if let Err(commit_err) = self.commit_prepared(&shared, token) {
            self.retire_control_session(&shared);
            let rollback = match previous_source.as_deref() {
                Some(old) => write_macro_file(id, old),
                None => match std::fs::remove_file(macro_path(id)) {
                    Ok(()) => Ok(()),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                    Err(e) => Err(e.to_string()),
                },
            };
            return match rollback {
                Ok(()) => Err(format!(
                    "macro '{id}' reload failed after persistence; previous revision restored: {commit_err}"
                )),
                Err(rollback_err) => Err(format!(
                    "macro '{id}' reload failed and durable rollback also failed: {commit_err}; rollback: {rollback_err}"
                )),
            };
        }

        {
            let mut g = self.inner.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            g.manifest.insert(id.to_string(), source.to_string());
            g.modes.insert(id.to_string(), mode);
            g.generations.insert(id.to_string(), generation);
            if let Some(old) = old_mode.filter(|old| *old != mode) {
                if let Some(s) = lane_session_mut(&mut g, old) {
                    let _ = s.send(&json!({"t": "unregister", "id": id}));
                }
            }
            if opts.is_array() {
                g.options.insert(id.to_string(), opts.clone());
            } else {
                g.options.remove(id);
            }
        }

        seed_option_defaults(id, &opts);
        Ok(())
    }

    /// Compile/execute a candidate namespace without publishing it into the live registry.
    fn prepare_register(
        &self,
        id: &str,
        source: &str,
        generation: u64,
        mode: MacroMode,
    ) -> Result<(u64, Value, Arc<Shared>), String> {
        let (rx, shared, rid, sent) = {
            let mut g = self.inner.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            self.ensure_lane_locked(&mut g, mode)?;
            let s = lane_session_mut(&mut g, mode).unwrap();
            let rid = s.shared.next_rid.fetch_add(1, Ordering::Relaxed);
            let (tx, rx) = channel();
            let shared = Arc::clone(&s.shared);
            shared
                .pending
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert(rid, tx);
            let sent = s.send(&json!({
                "t": "prepare_register",
                "rid": rid,
                "token": rid,
                "id": id,
                "source": source,
                "generation": generation,
            }));
            (rx, shared, rid, sent)
        };
        if !sent {
            shared
                .pending
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .remove(&rid);
            self.retire_control_session(&shared);
            return Err("sidecar pipe broken".into());
        }
        match rx.recv_timeout(FIRE_BUDGET) {
            Ok(v) if v.get("ok").and_then(Value::as_bool) == Some(true) => Ok((
                rid,
                v.get("options").cloned().unwrap_or(Value::Null),
                shared,
            )),
            Ok(v) => Err(v
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("register failed")
                .to_string()),
            Err(_) => {
                shared
                    .pending
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .remove(&rid);
                self.retire_control_session(&shared);
                Err("sidecar register timed out — session retired".into())
            }
        }
    }

    fn commit_prepared(&self, shared: &Arc<Shared>, token: u64) -> Result<(), String> {
        let (rx, rid, sent) = {
            let mut g = self.inner.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            let Some(mode) = lane_for_shared(&g, shared) else {
                return Err("prepared macro session was replaced before commit".into());
            };
            let s = lane_session_mut(&mut g, mode).unwrap();
            let rid = s.shared.next_rid.fetch_add(1, Ordering::Relaxed);
            let (tx, rx) = channel();
            shared
                .pending
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert(rid, tx);
            let sent = s.send(&json!({"t": "commit_register", "rid": rid, "token": token}));
            (rx, rid, sent)
        };
        if !sent {
            shared
                .pending
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .remove(&rid);
            return Err("sidecar pipe broke while committing macro".into());
        }
        match rx.recv_timeout(FIRE_BUDGET) {
            Ok(v) if v.get("ok").and_then(Value::as_bool) == Some(true) => Ok(()),
            Ok(v) => Err(v
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("macro commit failed")
                .to_string()),
            Err(_) => {
                shared
                    .pending
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .remove(&rid);
                Err("sidecar macro commit timed out".into())
            }
        }
    }

    fn discard_prepared(&self, shared: &Arc<Shared>, token: u64) {
        let mut g = self.inner.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(mode) = lane_for_shared(&g, shared) {
            if let Some(s) = lane_session_mut(&mut g, mode) {
                let _ = s.send(&json!({"t": "discard_register", "token": token}));
            }
        }
    }

    /// A macro's DECLARED options (its `NEURON_OPTIONS` manifest) — what the GUI renders controls
    /// from. `None` (or empty) = the macro takes no options.
    pub fn options_manifest(&self, id: &str) -> Option<Value> {
        self.inner.lock().unwrap_or_else(std::sync::PoisonError::into_inner).options.get(id).cloned()
    }

    /// The user's chosen option VALUES for a macro (a `{key: value}` map), from disk. Empty if none.
    pub fn option_values(&self, id: &str) -> Value {
        load_option_values(id)
    }

    /// Persist the user's chosen option values for a macro (the GUI's save path).
    pub fn set_option_values(&self, id: &str, values: &Value) -> Result<(), String> {
        validate_macro_id(id)?;
        write_option_values(id, values)
    }

    /// Delete one macro as runtime truth. Missing files are already-deleted success; a real source
    /// removal error leaves the live registry alone so a later respawn cannot resurrect hidden state.
    pub fn delete(&self, id: &str) -> Result<(), String> {
        validate_macro_id(id)?;
        let _mutation = self
            .mutations
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match std::fs::remove_file(macro_path(id)) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(format!("remove macro '{id}': {e}")),
        }
        let _ = std::fs::remove_file(options_path(id));
        invalidate_option_cache(id);
        self.forget_live(id);
        Ok(())
    }

    /// Best-effort teardown used heavily by tests.
    pub fn unregister(&self, id: &str) {
        let _ = self.delete(id);
    }

    fn forget_live(&self, id: &str) {
        let mut g = self.inner.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        g.manifest.remove(id);
        g.modes.remove(id);
        g.generations.remove(id);
        g.options.remove(id);
        for mode in [MacroMode::Raw, MacroMode::Bound] {
            if let Some(s) = lane_session_mut(&mut g, mode) {
                let _ = s.send(&json!({"t": "unregister", "id": id}));
            }
        }
    }

    /// Syntax-check + list top-level defs WITHOUT executing (the honest "dry-run" — Python is
    /// full-power, so we never claim a behavioural trace). Returns the def names on success.
    pub fn check(&self, source: &str) -> Result<Vec<String>, String> {
        let (rx, shared, rid, rid_send) = {
            let mut g = self.inner.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            self.ensure_lane_locked(&mut g, MacroMode::Raw)?;
            let s = lane_session_mut(&mut g, MacroMode::Raw).unwrap();
            let rid = s.shared.next_rid.fetch_add(1, Ordering::Relaxed);
            let (tx, rx) = channel();
            let shared = Arc::clone(&s.shared);
            shared.pending.lock().unwrap_or_else(std::sync::PoisonError::into_inner).insert(rid, tx);
            let ok = s.send(&json!({"t": "check", "rid": rid, "source": source}));
            (rx, shared, rid, ok)
        };
        if !rid_send {
            shared.pending.lock().unwrap_or_else(std::sync::PoisonError::into_inner).remove(&rid);
            self.retire_control_session(&shared);
            return Err("sidecar pipe broken".into());
        }
        match rx.recv_timeout(FIRE_BUDGET) {
            Ok(v) if v.get("ok").and_then(Value::as_bool) == Some(true) => Ok(v
                .get("defs")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(|x| x.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default()),
            Ok(v) => Err(v
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("syntax error")
                .to_string()),
            Err(_) => {
                shared.pending.lock().unwrap_or_else(std::sync::PoisonError::into_inner).remove(&rid);
                self.retire_control_session(&shared);
                Err("sidecar check timed out — session retired".into())
            }
        }
    }

    /// Parse a whole Python macro module for the visual constructor. Python owns the grammar and
    /// returns the typed entry-body nodes plus exact source around/at that entry; Rust owns the
    /// Neuron compiler directive and never executes source merely to discover authority.
    pub fn parse_document(&self, source: &str) -> DocumentParseResult {
        let mode = mode_from_source(source).map_err(|msg| ParseError::Policy { line: 1, msg })?;
        let (rx, shared, rid, sent) = {
            let mut g = self.inner.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            self.ensure_lane_locked(&mut g, MacroMode::Raw).map_err(host_err)?;
            let s = lane_session_mut(&mut g, MacroMode::Raw).unwrap();
            let rid = s.shared.next_rid.fetch_add(1, Ordering::Relaxed);
            let (tx, rx) = channel();
            let shared = Arc::clone(&s.shared);
            shared.pending.lock().unwrap_or_else(std::sync::PoisonError::into_inner).insert(rid, tx);
            let ok = s.send(&json!({"t": "parse", "rid": rid, "source": source}));
            (rx, shared, rid, ok)
        };
        if !sent {
            shared.pending.lock().unwrap_or_else(std::sync::PoisonError::into_inner).remove(&rid);
            self.retire_control_session(&shared);
            return Err(host_err("sidecar pipe broken"));
        }
        match rx.recv_timeout(FIRE_BUDGET) {
            Ok(v) if v.get("ok").and_then(Value::as_bool) == Some(true) => {
                let nodes = v.get("nodes").cloned().unwrap_or(Value::Null);
                let body = serde_json::from_value::<Vec<MacroNode>>(nodes)
                    .map_err(|e| host_err(format!("sidecar returned malformed node JSON: {e}")))?;
                Ok(MacroDocument {
                    mode,
                    prefix: v.get("prefix").and_then(Value::as_str).unwrap_or("").to_string(),
                    header: v.get("header").and_then(Value::as_str).unwrap_or("").to_string(),
                    body,
                    suffix: v.get("suffix").and_then(Value::as_str).unwrap_or("").to_string(),
                })
            }
            Ok(v) => {
                let err = v.get("error");
                let line = err.and_then(|e| e.get("line")).and_then(Value::as_u64).unwrap_or(0) as u32;
                let msg = err.and_then(|e| e.get("msg")).and_then(Value::as_str)
                    .unwrap_or("syntax error").to_string();
                Err(ParseError::Syntax { line, msg })
            }
            Err(_) => {
                shared.pending.lock().unwrap_or_else(std::sync::PoisonError::into_inner).remove(&rid);
                self.retire_control_session(&shared);
                Err(host_err("sidecar parse timed out — session retired"))
            }
        }
    }

    /// Body-only compatibility view over parse_document.
    pub fn parse_macro(&self, source: &str) -> ParseResult {
        self.parse_document(source).map(|doc| doc.body)
    }

    /// Run source once without registering or persisting it. This is the editor/CLI candidate path:
    /// module top-level code and the macro body execute on a bounded disposable Python worker, while
    /// the live registry and on-disk script remain untouched.
    pub fn invoke_source(
        &self,
        id: &str,
        source: &str,
        ctx: &crate::macros::context::Context,
    ) -> String {
        self.invoke_source_with_budget(id, source, ctx, FIRE_BUDGET)
    }

    /// invoke_source with an explicit human-scale wait budget.
    pub fn invoke_source_with_budget(
        &self,
        id: &str,
        source: &str,
        ctx: &crate::macros::context::Context,
        budget: Duration,
    ) -> String {
        let mode = match mode_from_source(source) {
            Ok(mode) => mode,
            Err(e) => return format!("[macro policy error: {e}]"),
        };
        let (rx, shared, rid, sent) = {
            let mut g = self.inner.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Err(e) = self.ensure_lane_locked(&mut g, mode) {
                return format!("[{e}]");
            }
            let armed = self.armed.load(Ordering::SeqCst);
            let s = lane_session_mut(&mut g, mode).unwrap();
            let rid = s.shared.next_rid.fetch_add(1, Ordering::Relaxed);
            let (tx, rx) = channel();
            let shared = Arc::clone(&s.shared);
            shared.pending.lock().unwrap_or_else(std::sync::PoisonError::into_inner).insert(rid, tx);
            let options = if validate_macro_id(id).is_ok() {
                load_option_values(id)
            } else {
                json!({})
            };
            let sent = s.send(&json!({
                "t": "fire_source",
                "rid": rid,
                "id": id,
                "source": source,
                "ctx": ctx_json(ctx, armed),
                "options": options,
                "mock": false,
            }));
            (rx, shared, rid, sent)
        };
        if !sent {
            shared.pending.lock().unwrap_or_else(std::sync::PoisonError::into_inner).remove(&rid);
            self.retire_control_session(&shared);
            return "[sidecar pipe broken]".into();
        }
        match rx.recv_timeout(budget) {
            Ok(v) if v.get("ok").and_then(Value::as_bool) == Some(true) => v
                .get("value")
                .and_then(Value::as_str)
                .map(|s| format!("macro '{id}': {s}"))
                .unwrap_or_else(|| format!("macro '{id}' ran")),
            Ok(v) => format!(
                "macro '{id}' error: {}",
                v.get("error")
                    .and_then(Value::as_str)
                    .unwrap_or("(no detail)")
            ),
            Err(_) => {
                shared.pending.lock().unwrap_or_else(std::sync::PoisonError::into_inner).remove(&rid);
                format!(
                    "macro '{id}' still running (waiting on a beacon or a slow call?) — \
                     result will land in the macro log"
                )
            }
        }
    }

    /// Mock-fire unsaved source without entering the registry.
    pub fn fire_source_mock(
        &self,
        id: &str,
        source: &str,
        ctx: &crate::macros::context::Context,
    ) -> String {
        self.fire_source_dispatch(id, source, ctx, true)
    }

    fn fire_source_dispatch(
        &self,
        id: &str,
        source: &str,
        ctx: &crate::macros::context::Context,
        mock: bool,
    ) -> String {
        crate::prof::bump(&crate::prof::MACRO_FIRE);
        let mode = match mode_from_source(source) {
            Ok(mode) => mode,
            Err(e) => return format!("[macro policy error: {e}]"),
        };
        let armed = !mock && self.armed.load(Ordering::SeqCst);
        if let Ok(mut g) = self.inner.try_lock() {
            let warm = lane_session(&g, mode)
                .map(|s| {
                    let st = s.shared.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                    st.warm && !st.dead
                })
                .unwrap_or(false);
            if warm {
                let s = lane_session_mut(&mut g, mode).unwrap();
                let rid = s.shared.next_rid.fetch_add(1, Ordering::Relaxed);
                let options = if validate_macro_id(id).is_ok() {
                    load_option_values(id)
                } else {
                    json!({})
                };
                if s.send(&json!({
                    "t": "fire_source",
                    "rid": rid,
                    "id": id,
                    "source": source,
                    "ctx": ctx_json(ctx, armed),
                    "options": options,
                    "mock": mock,
                })) {
                    return format!("macro '{id}' source dispatched");
                }
                let doomed = lane_take(&mut g, mode);
                drop(g);
                drop(doomed);
                self.spawn_background_warm(mode);
                return format!("macro '{id}' source dropped (sidecar died — warming, press again)");
            }
            drop(g);
        }
        self.spawn_background_warm(mode);
        if !self.available() {
            "[python runtime unavailable]".into()
        } else {
            format!("macro '{id}' source — sidecar warming, press again")
        }
    }

    /// FIRE A MACRO AND RETURN IMMEDIATELY — the live-dispatch path. NEVER blocks the caller (the
    /// 1000 Hz input thread): it `try_lock`s the inner mutex so a concurrent cold spawn can never
    /// stall it, sends the fire frame if warm, and otherwise kicks a (deduplicated) background warm.
    /// The result lands asynchronously in the log ring. Returns a one-line status for the readout.
    pub fn fire_async(&self, id: &str, ctx: &crate::macros::context::Context) -> String {
        self.fire_dispatch(id, ctx, false)
    }

    /// MOCK-FIRE — run the macro with input forced DISARMED for THIS fire only, so every effectful
    /// helper (`key`/`type_text`/`run`/…) no-ops while `neuron.ask`/`notify` still reach the human.
    /// The macro's REAL beacon rises through the live presenter; none of its real actions land. Same
    /// non-blocking contract as [`fire_async`] — the GUI's beacon "test" button. Never moves the global
    /// arm gate (the `mock` flag is per-fire in the sidecar), so concurrent live macros are untouched.
    pub fn fire_mock(&self, id: &str, ctx: &crate::macros::context::Context) -> String {
        self.fire_dispatch(id, ctx, true)
    }

    /// Shared non-blocking fire path. `mock` forces this fire disarmed (and tags the frame so the
    /// sidecar suppresses every side effect for that worker) without touching the global arm state.
    fn fire_dispatch(
        &self,
        id: &str,
        ctx: &crate::macros::context::Context,
        mock: bool,
    ) -> String {
        crate::prof::bump(&crate::prof::MACRO_FIRE);
        let armed = !mock && self.armed.load(Ordering::SeqCst);
        if let Ok(mut g) = self.inner.try_lock() {
            if let Some(mode) = g.modes.get(id).copied() {
                let warm = lane_session(&g, mode)
                    .map(|s| {
                        let st = s.shared.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                        st.warm && !st.dead
                    })
                    .unwrap_or(false);
                if warm {
                    let generation = g.generations.get(id).copied().unwrap_or(0);
                    let s = lane_session_mut(&mut g, mode).unwrap();
                    let rid = s.shared.next_rid.fetch_add(1, Ordering::Relaxed);
                    if s.send(&json!({
                        "t": "fire", "rid": rid, "id": id, "generation": generation,
                        "ctx": ctx_json(ctx, armed), "options": load_option_values(id), "mock": mock
                    })) {
                        return format!("macro '{id}' dispatched");
                    }
                    let doomed = lane_take(&mut g, mode);
                    drop(g);
                    drop(doomed);
                    self.spawn_background_warm(mode);
                    return format!("macro '{id}' dropped (sidecar died — warming, press again)");
                }
                drop(g);
                self.spawn_background_warm(mode);
            } else {
                drop(g);
                self.spawn_background_warm(MacroMode::Raw);
                self.spawn_background_warm(MacroMode::Bound);
            }
        } else {
            self.spawn_background_warm(MacroMode::Raw);
            self.spawn_background_warm(MacroMode::Bound);
        }
        if !self.available() {
            "[python runtime unavailable]".into()
        } else {
            format!("macro '{id}' — sidecar warming, press again")
        }
    }

    /// Run a macro and WAIT for its result (bounded by [`FIRE_BUDGET`]) — the GUI "test run".
    /// Never call from the input/UI thread.
    pub fn invoke(&self, id: &str, ctx: &crate::macros::context::Context) -> String {
        self.invoke_with_budget(id, ctx, FIRE_BUDGET)
    }

    /// [`invoke`](MacroHost::invoke) with an explicit wait budget. The CLI's `macro run` passes a
    /// generous one so a macro that BLOCKS ON A BEACON (`neuron.ask`) can wait for the human —
    /// the terminal prompt is answered on no machine timescale. Ensures the sidecar is warm first.
    pub fn invoke_with_budget(
        &self,
        id: &str,
        ctx: &crate::macros::context::Context,
        budget: Duration,
    ) -> String {
        let (rx, shared, rid, sent) = {
            let mut g = self.inner.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            self.sync_manifest(&mut g);
            let mode = g.modes.get(id).copied().unwrap_or(MacroMode::Raw);
            if let Err(e) = self.ensure_lane_locked(&mut g, mode) {
                return format!("[{e}]");
            }
            let armed = self.armed.load(Ordering::SeqCst);
            let generation = g.generations.get(id).copied().unwrap_or(0);
            let s = lane_session_mut(&mut g, mode).unwrap();
            let rid = s.shared.next_rid.fetch_add(1, Ordering::Relaxed);
            let (tx, rx) = channel();
            let shared = Arc::clone(&s.shared);
            // Race window: a concurrent mark_dead's clear can land in the gap right here — see
            // `Shared::mark_dead`'s two failpoints and the `macro_host_death_race_*` tests, which
            // freeze one side or the other of this exact window.
            crate::failpoint!("macro_host.pending_insert.before");
            shared.pending.lock().unwrap_or_else(std::sync::PoisonError::into_inner).insert(rid, tx);
            let ok = s.send(&json!({"t": "fire", "rid": rid, "id": id, "generation": generation, "ctx": ctx_json(ctx, armed), "options": load_option_values(id)}));
            (rx, shared, rid, ok)
        };
        if !sent {
            shared.pending.lock().unwrap_or_else(std::sync::PoisonError::into_inner).remove(&rid);
            return "[sidecar pipe broken]".into();
        }
        match rx.recv_timeout(budget) {
            Ok(v) if v.get("ok").and_then(Value::as_bool) == Some(true) => v
                .get("value")
                .and_then(Value::as_str)
                .map(|s| format!("macro '{id}': {s}"))
                .unwrap_or_else(|| format!("macro '{id}' ran")),
            Ok(v) => format!(
                "macro '{id}' error: {}",
                v.get("error")
                    .and_then(Value::as_str)
                    .unwrap_or("(no detail)")
            ),
            // The wait expired, NOT necessarily the macro: a slow API call or an unanswered beacon
            // keeps running on its sidecar worker — its result lands in the macro log when it ends.
            Err(_) => {
                shared.pending.lock().unwrap_or_else(std::sync::PoisonError::into_inner).remove(&rid);
                format!(
                    "macro '{id}' still running (waiting on a beacon or a slow call?) — \
                     result will land in the macro log"
                )
            }
        }
    }

    /// Spawn + warm the sidecar at app launch (off the UI thread). Idempotent; errors are
    /// returned for surfacing but never fatal (the macro tier just stays disabled). The on-disk
    /// macro scan lives in the spawn path itself (see [`ensure_locked`]), so EVERY road to a warm
    /// sidecar — GUI launch, `macro run <name>`, a respawn after a crash — sees the same world.
    pub fn ensure_warm(&self) -> Result<(), String> {
        let mut g = self.inner.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        self.sync_manifest(&mut g);
        self.ensure_lane_locked(&mut g, MacroMode::Raw)?;
        if g.modes.values().any(|m| *m == MacroMode::Bound) {
            self.ensure_lane_locked(&mut g, MacroMode::Bound)?;
        }
        Ok(())
    }

    /// Drain the macro-log ring (the sidecar's stderr: prints + tracebacks). Newest last. Reads the
    /// persistent MacroHost-level ring, so a crashed sidecar's final lines survive its respawn.
    pub fn drain_log(&self) -> Vec<String> {
        self.log.lock().unwrap_or_else(std::sync::PoisonError::into_inner).drain(..).collect()
    }

    // ── internals ──────────────────────────────────────────────────────────────────────────

    /// A control-plane timeout means one Python main loop is no longer trustworthy. Retire exactly
    /// the execution domain that owned the timed-out Shared.
    fn retire_control_session(&self, shared: &Arc<Shared>) {
        let doomed = {
            let mut g = self.inner.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(mode) = lane_for_shared(&g, shared) {
                lane_breaker_mut(&mut g, mode).record_crash();
                lane_take(&mut g, mode)
            } else {
                None
            }
        };
        drop(doomed);
    }

    fn sync_manifest(&self, g: &mut Inner) {
        for (id, src) in scan_macro_dir() {
            if !g.manifest.contains_key(&id) {
                let Ok(mode) = mode_from_source(&src) else {
                    eprintln!("[macro] '{id}' has invalid execution policy; skipped until edited");
                    continue;
                };
                let generation = self.next_generation.fetch_add(1, Ordering::Relaxed);
                g.manifest.insert(id.clone(), src);
                g.modes.insert(id.clone(), mode);
                g.generations.insert(id, generation);
            } else {
                if !g.generations.contains_key(&id) {
                    let generation = self.next_generation.fetch_add(1, Ordering::Relaxed);
                    g.generations.insert(id.clone(), generation);
                }
                if !g.modes.contains_key(&id) {
                    if let Some(src) = g.manifest.get(&id) {
                        if let Ok(mode) = mode_from_source(src) {
                            g.modes.insert(id.clone(), mode);
                        }
                    }
                }
            }
        }
    }

    /// Ensure one execution domain is warm. RAW and BOUND never share a Python process.
    fn ensure_lane_locked(&self, g: &mut Inner, mode: MacroMode) -> Result<(), String> {
        self.sync_manifest(g);
        let dead = lane_session(g, mode)
            .map(|s| s.shared.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner).dead)
            .unwrap_or(true);
        if !dead {
            return Ok(());
        }
        if lane_session(g, mode).is_some() {
            lane_breaker_mut(g, mode).record_crash();
            let old = lane_take(g, mode);
            drop(old);
        }
        if lane_breaker_mut(g, mode).tripped() {
            return Err(format!(
                "{} macro sidecar disabled (crashed repeatedly — re-enable in Settings)",
                mode.label()
            ));
        }
        let session = match spawn_session(
            mode,
            self.armed.load(Ordering::SeqCst),
            self.log.clone(),
            self.beacon.clone(),
        ) {
            Ok(s) => s,
            Err(e) => {
                lane_breaker_mut(g, mode).record_crash();
                return Err(e);
            }
        };
        lane_breaker_mut(g, mode).reset();

        let mut sess = session;
        for (id, src) in &g.manifest {
            if g.modes.get(id).copied() != Some(mode) {
                continue;
            }
            let generation = g.generations.get(id).copied().unwrap_or(0);
            let rid = sess.shared.next_rid.fetch_add(1, Ordering::Relaxed);
            let _ = sess.send(&json!({
                "t": "register", "rid": rid, "id": id, "source": src, "generation": generation,
            }));
        }
        crate::failpoint!("macro_host.ensure_locked.before_replace");
        lane_set(g, mode, sess);
        Ok(())
    }

    fn warm_latch(&self, mode: MacroMode) -> &AtomicBool {
        match mode {
            MacroMode::Raw => &self.warm_in_flight,
            MacroMode::Bound => &self.bound_warm_in_flight,
        }
    }

    fn spawn_background_warm(&self, mode: MacroMode) {
        let me: &'static MacroHost = macro_host();
        if me
            .warm_latch(mode)
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return;
        }
        let label = match mode {
            MacroMode::Raw => "macro-host-warm-raw",
            MacroMode::Bound => "macro-host-warm-bound",
        };
        crate::worker::spawn_guarded(
            label,
            move || me.warm_latch(mode).store(false, Ordering::Release),
            move || {
                let mut g = me.inner.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                let _ = me.ensure_lane_locked(&mut g, mode);
            },
        );
    }

}

/// Spawn one sidecar process + its reader/logger threads, and wait until the `ready` frame arrives.
/// `log` is the persistent MacroHost-level ring this session feeds (so its output outlives it);
/// `beacon` is the Macro Host-level prompt-listener slot the reader routes ask/notify frames to.
fn spawn_session(
    mode: MacroMode,
    armed: bool,
    log: LogRing,
    beacon: BeaconSlot,
) -> Result<Session, String> {
    let rt = resolve_runtime()?;

    let mut cmd = Command::new(&rt.python);
    cmd.arg(&rt.host_script)
        .env("NEURON_MACRO_MODE", match mode {
            MacroMode::Raw => "raw",
            MacroMode::Bound => "bound",
        })
        .current_dir(&rt.host_dir) // so `import neuron` finds the co-located host/neuron.py
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(windows)]
    cmd.creation_flags(DETACHED_PROCESS);

    let mut child = cmd
        .spawn()
        .map_err(|e| format!("spawn python sidecar: {e}"))?;

    // Keep the legacy profiler slot pointed at RAW. BOUND has independent process lifetime.
    if mode == MacroMode::Raw {
        crate::prof::SIDECAR_PID.store(child.id(), std::sync::atomic::Ordering::Relaxed);
    }

    let stdin = Arc::new(Mutex::new(child.stdin.take().ok_or("no child stdin")?));
    let stdout = child.stdout.take().ok_or("no child stdout")?;
    let stderr = child.stderr.take().ok_or("no child stderr")?;

    let shared = Arc::new(Shared {
        pending: Mutex::new(HashMap::new()),
        state: Mutex::new(LinkState::default()),
        cv: Condvar::new(),
        log,
        next_rid: AtomicU64::new(1),
    });

    // reader thread: protocol frames off child STDOUT. Keep its handle so Drop can join it. It
    // holds the stdin WEAKLY so a dropped Session's pipe really closes (the reader must not keep
    // the child's stdin alive past the session's death).
    // Session owns the reader/logger handles and joins them on Drop, so both route through the
    // handle-returning primitive.
    let reader = {
        let shared = shared.clone();
        let stdin_weak = Arc::downgrade(&stdin);
        let name = match mode {
            MacroMode::Raw => "macro-host-reader-raw",
            MacroMode::Bound => "macro-host-reader-bound",
        };
        crate::worker::spawn_named(name, move || {
            reader_loop(stdout, shared, beacon, stdin_weak, mode)
        })
        .ok()
    };
    // logger thread: macro output off child STDERR -> bounded ring.
    let logger = {
        let shared = shared.clone();
        let name = match mode {
            MacroMode::Raw => "macro-host-logger-raw",
            MacroMode::Bound => "macro-host-logger-bound",
        };
        crate::worker::spawn_named(name, move || {
                let mut r = BufReader::new(stderr);
                let mut line = Vec::new();
                let mut byte = [0u8; 1];
                loop {
                    match r.read(&mut byte) {
                        Ok(0) | Err(_) => break,
                        Ok(_) => {
                            if byte[0] == b'\n' {
                                shared.push_log(String::from_utf8_lossy(&line).into_owned());
                                line.clear();
                            } else if line.len() < 8192 {
                                line.push(byte[0]);
                            }
                        }
                    }
                }
                if !line.is_empty() {
                    shared.push_log(String::from_utf8_lossy(&line).into_owned());
                }
            })
            .ok()
    };

    // wait for warm (the `ready` frame the reader sets), bounded.
    {
        let mut st = shared.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let deadline = Instant::now() + WARM_TIMEOUT;
        while !st.warm && !st.dead {
            let now = Instant::now();
            if now >= deadline {
                break;
            }
            let (g, _) = shared.cv.wait_timeout(st, deadline - now).unwrap();
            st = g;
        }
        if !st.warm {
            drop(st);
            let _ = child.kill();
            let _ = child.wait();
            return Err("python sidecar did not come ready".into());
        }
    }

    let mut sess = Session {
        child,
        stdin,
        shared,
        reader,
        logger,
    };
    // push the current arm state before any fire can be accepted.
    let _ = sess.send(&json!({"t": "armed", "on": armed}));
    Ok(sess)
}

/// Deliver a beacon event to the installed listener, returning whether anyone took it. A dead
/// receiver (the UI dropped its end) clears the slot so later prompts take the no-UI path cleanly.
fn beacon_deliver(beacon: &BeaconSlot, ev: BeaconEvent) -> bool {
    let mut slot = beacon.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    match slot.as_ref() {
        Some(tx) if tx.send(ev).is_ok() => true,
        Some(_) => {
            *slot = None;
            false
        }
        None => false,
    }
}

/// The device registry, loaded ONCE for macro device-verbs (`neuron.dpi/profile/…`) and cached, so a
/// macro spamming acts never re-reads the registry TOMLs.
fn act_registry() -> Option<&'static crate::registry::Registry> {
    static R: std::sync::OnceLock<Option<crate::registry::Registry>> = std::sync::OnceLock::new();
    R.get_or_init(|| crate::registry::Registry::load().ok())
        .as_ref()
}

/// Open the first connected device that satisfies `cap` (for a macro's read-back / brightness verb).
fn open_capable(cap: crate::registry::Capability) -> Option<crate::device::Device> {
    let reg = act_registry()?;
    reg.devices
        .iter()
        .filter(|d| d.supports(cap))
        .find_map(|d| {
            d.product_ids()
                .find_map(|pid| crate::device::Device::open(d.clone(), pid).ok())
        })
}

/// Run a macro's device/profile/system VERB. Writes go through the SHARED intent path — so
/// `neuron.dpi(1600)` is the exact same write (and confirmation card) as a bound trigger or the CLI,
/// one source of truth. Reads (`battery`/`current_dpi`/`active_profile`/`scroll_stage`) let a macro
/// SENSE live state and react. Audio + brightness reuse the same Core-Audio / capability code the
/// bound actions use. Returns `(ok, message)` — for a read, the message IS the value.

fn macro_state_dir() -> PathBuf {
    if let Some(p) = std::env::var_os("NEURON_MACRO_STATE") {
        return PathBuf::from(p);
    }
    dirs::data_local_dir()
        .unwrap_or_else(|| std::env::temp_dir().join("neuron-data"))
        .join("neuron")
        .join("macro_state")
}

fn macro_state_path(id: &str) -> PathBuf {
    macro_state_dir().join(format!("{id}.json"))
}

fn macro_state_lock(id: &str) -> Arc<Mutex<()>> {
    static LOCKS: OnceLock<Mutex<HashMap<String, Arc<Mutex<()>>>>> = OnceLock::new();
    let locks = LOCKS.get_or_init(|| Mutex::new(HashMap::new()));
    locks
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .entry(id.to_string())
        .or_insert_with(|| Arc::new(Mutex::new(())))
        .clone()
}

fn macro_state_read(id: &str) -> serde_json::Map<String, Value> {
    std::fs::read_to_string(macro_state_path(id))
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .and_then(|v| v.as_object().cloned())
        .unwrap_or_default()
}

fn macro_state_write(id: &str, map: &serde_json::Map<String, Value>) -> Result<(), String> {
    let path = macro_state_path(id);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let body = serde_json::to_vec(map).map_err(|e| e.to_string())?;
    let mut last = None;
    for attempt in 0..40 {
        match crate::salvage::atomic_write(&path, &body) {
            Ok(()) => return Ok(()),
            Err(e) => {
                let transient = cfg!(windows) && matches!(e.raw_os_error(), Some(5 | 32));
                if !transient || attempt == 39 {
                    return Err(e.to_string());
                }
                last = Some(e);
                std::thread::sleep(Duration::from_millis(25));
            }
        }
    }
    Err(last.map(|e| e.to_string()).unwrap_or_else(|| "state write failed".into()))
}

fn run_state_act(id: &str, verb: &str, arg: &Value) -> Option<(bool, String)> {
    if !verb.starts_with("state_") {
        return None;
    }
    if validate_macro_id(id).is_err() {
        return Some((false, "invalid macro id for state".into()));
    }
    let lock = macro_state_lock(id);
    let _guard = lock.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    match verb {
        "state_store" => {
            let Some(key) = arg.get("key").and_then(Value::as_str) else {
                return Some((false, "state_store: missing key".into()));
            };
            let value = arg.get("value").cloned().unwrap_or(Value::Null);
            let mut map = macro_state_read(id);
            map.insert(key.to_string(), value);
            Some(match macro_state_write(id, &map) {
                Ok(()) => (true, "true".into()),
                Err(e) => (false, format!("state write failed: {e}")),
            })
        }
        "state_load" => {
            let Some(key) = arg.get("key").and_then(Value::as_str) else {
                return Some((false, "state_load: missing key".into()));
            };
            let map = macro_state_read(id);
            let payload = match map.get(key) {
                Some(v) => json!({"found": true, "value": v}),
                None => json!({"found": false}),
            };
            Some((true, payload.to_string()))
        }
        "state_forget" => {
            if arg.is_null() {
                let ok = match std::fs::remove_file(macro_state_path(id)) {
                    Ok(()) => true,
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => true,
                    Err(_) => false,
                };
                return Some((ok, if ok { "true" } else { "false" }.into()));
            }
            let Some(key) = arg.as_str() else {
                return Some((false, "state_forget: key must be a string or null".into()));
            };
            let mut map = macro_state_read(id);
            map.remove(key);
            Some(match macro_state_write(id, &map) {
                Ok(()) => (true, "true".into()),
                Err(e) => (false, format!("state write failed: {e}")),
            })
        }
        "state_stored" => {
            let map = macro_state_read(id);
            Some((true, Value::Object(map).to_string()))
        }
        _ => Some((false, format!("unknown state verb '{verb}'"))),
    }
}


fn run_cross_invoke(
    caller_mode: MacroMode,
    target: &str,
    arg: &Value,
    mock: bool,
) -> (bool, String) {
    if validate_macro_id(target).is_err() {
        return (true, json!({"found": false}).to_string());
    }
    let wait = arg.get("wait").and_then(Value::as_bool).unwrap_or(true);
    let ctx = arg.get("ctx").cloned().unwrap_or_else(|| json!({}));
    let options = arg.get("options").cloned().unwrap_or_else(|| json!({}));

    let host = macro_host();
    let prepared = {
        let mut g = host.inner.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        host.sync_manifest(&mut g);
        let Some(target_mode) = g.modes.get(target).copied() else {
            return (true, json!({"found": false}).to_string());
        };
        if caller_mode == MacroMode::Bound && target_mode == MacroMode::Raw {
            return (
                false,
                json!({"found": true, "error": "BOUND cannot invoke RAW"}).to_string(),
            );
        }
        if let Err(e) = host.ensure_lane_locked(&mut g, target_mode) {
            return (false, json!({"found": true, "error": e}).to_string());
        }
        let generation = g.generations.get(target).copied().unwrap_or(0);
        let s = lane_session_mut(&mut g, target_mode).unwrap();
        let rid = s.shared.next_rid.fetch_add(1, Ordering::Relaxed);
        let shared = Arc::clone(&s.shared);
        if wait {
            let (tx, rx) = channel();
            shared
                .pending
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert(rid, tx);
            let sent = s.send(&json!({
                "t": "fire",
                "rid": rid,
                "id": target,
                "generation": generation,
                "ctx": ctx,
                "options": options,
                "mock": mock,
            }));
            (Some(rx), shared, rid, sent)
        } else {
            let sent = s.send(&json!({
                "t": "fire",
                "rid": Value::Null,
                "id": target,
                "generation": generation,
                "ctx": ctx,
                "options": options,
                "mock": mock,
            }));
            (None, shared, rid, sent)
        }
    };

    let (rx, shared, rid, sent) = prepared;
    if !sent {
        if rx.is_some() {
            shared
                .pending
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .remove(&rid);
        }
        host.retire_control_session(&shared);
        return (
            false,
            json!({"found": true, "error": "target sidecar pipe broken"}).to_string(),
        );
    }
    let Some(rx) = rx else {
        return (true, json!({"found": true, "queued": true}).to_string());
    };

    match rx.recv_timeout(Duration::from_secs(300)) {
        Ok(v) if v.get("ok").and_then(Value::as_bool) == Some(true) => (
            true,
            json!({
                "found": true,
                "ok": true,
                "value": v.get("value").cloned().unwrap_or(Value::Null),
            })
            .to_string(),
        ),
        Ok(v) => (
            true,
            json!({
                "found": true,
                "ok": false,
                "error": v.get("error").and_then(Value::as_str).unwrap_or("invoked macro failed"),
            })
            .to_string(),
        ),
        Err(_) => {
            shared
                .pending
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .remove(&rid);
            (
                false,
                json!({"found": true, "error": "cross-domain invoke timed out"}).to_string(),
            )
        }
    }
}

fn run_act(_mode: MacroMode, _macro_id: &str, verb: &str, arg: &Value, mock: bool) -> (bool, String) {
    use crate::action::{Direction, Intent};
    use crate::capability as cap;

    // ── SENSE: read-back verbs (never gated — reading state has no side effect) ──────────────────
    match verb {
        "active_profile" => return (true, crate::profile::active()),
        "scroll_stage" => return (true, crate::writes::scroll_stage_cursor().to_string()),
        "battery" => {
            return match open_capable(crate::registry::Capability::Battery) {
                Some(d) => match cap::battery_percent(&d) {
                    Ok(p) => (true, format!("{}|{}", p, cap::charging(&d).unwrap_or(false))),
                    Err(e) => (false, format!("battery read failed: {e}")),
                },
                None => (false, "no battery-capable device".into()),
            }
        }
        "current_dpi" => {
            return match open_capable(crate::registry::Capability::Dpi) {
                Some(d) => match cap::dpi(&d) {
                    Ok((x, _)) => (true, x.to_string()),
                    Err(e) => (false, format!("dpi read failed: {e}")),
                },
                None => (false, "no dpi-capable device".into()),
            }
        }
        _ => {}
    }

    if let Some(result) = run_state_act(_macro_id, verb, arg) {
        return result;
    }
    if verb == "invoke" {
        let Some(target) = arg.get("id").and_then(Value::as_str) else {
            return (false, json!({"found": false}).to_string());
        };
        return run_cross_invoke(_mode, target, arg, mock);
    }

    // The host is the authority boundary. Python's helper-side gate is useful fast feedback, but a
    // forged act frame must still be unable to synthesize input, mutate the clipboard/audio/device,
    // focus windows or drive an external integration while SAFE mode is active.
    let read_only = matches!(
        verb,
        "active_profile" | "scroll_stage" | "battery" | "current_dpi" | "clipboard_get" | "obs_get" | "signal"
    );
    if !read_only && (mock || !crate::action::input_armed()) {
        return (false, "[disarmed]".into());
    }

    // ── BOUND HOST EFFECTS: same native input/clipboard/focus primitives Neuron already owns ─────
    let native = match verb {
        "key" => arg.as_str().map(crate::action::macro_key),
        "key_down" => arg.as_str().map(crate::action::macro_key_down),
        "key_up" => arg.as_str().map(crate::action::macro_key_up),
        "hotkey" => arg.as_array().map(|a| {
            let keys: Vec<String> = a.iter().filter_map(Value::as_str).map(str::to_string).collect();
            crate::action::macro_hotkey(&keys)
        }),
        "type_text" => arg.as_str().map(crate::action::macro_type_text),
        "type_ghost" => {
            let text = arg.get("text").and_then(Value::as_str);
            let speed = arg.get("speed").and_then(Value::as_str).unwrap_or("borderline");
            text.map(|text| crate::action::macro_type_ghost(text, speed))
        }
        "click" => arg.as_str().map(crate::action::macro_click),
        "scroll" => arg.as_i64().map(|n| crate::action::macro_scroll(n.clamp(i32::MIN as i64, i32::MAX as i64) as i32)),
        "mouse_move" => {
            let dx = arg.get("dx").and_then(Value::as_i64);
            let dy = arg.get("dy").and_then(Value::as_i64);
            match (dx, dy) {
                (Some(dx), Some(dy)) => Some(crate::action::macro_mouse_move(dx as i32, dy as i32)),
                _ => None,
            }
        }
        "mouse_to" => {
            let x = arg.get("x").and_then(Value::as_i64);
            let y = arg.get("y").and_then(Value::as_i64);
            match (x, y) {
                (Some(x), Some(y)) => Some(crate::action::macro_mouse_to(x as i32, y as i32)),
                _ => None,
            }
        }
        "clipboard_get" => return (true, crate::pocket::macro_clipboard_get().unwrap_or_default()),
        "clipboard_set" => {
            return match arg.as_str() {
                Some(text) if crate::pocket::macro_clipboard_set(text) => (true, "ok".into()),
                Some(_) => (false, "clipboard write failed".into()),
                None => (false, "clipboard_set(text): text must be a string".into()),
            }
        }
        "focus" => arg.as_str().map(crate::macros::context::focus_title),
        _ => None,
    };
    if let Some(msg) = native {
        let ok = !msg.starts_with('[') && !msg.starts_with("unknown") && !msg.ends_with("failed");
        return (ok, msg);
    }

    // ── AUDIO: the system mixer (mic capture + output render), via Core Audio ────────────────────
    if let Some((render, gain)) = match verb {
        "mic_mute" => Some((false, false)),
        "out_mute" => Some((true, false)),
        "mic_gain" => Some((false, true)),
        "out_gain" => Some((true, true)),
        _ => None,
    } {
        let ep = if render {
            crate::audio::resolve_render(None)
        } else {
            crate::audio::resolve_capture(None)
        };
        let Some(ep) = ep else {
            return (false, "no audio endpoint".into());
        };
        let Some(ctl) = crate::audio::VolumeCtl::open(&ep.id) else {
            return (false, "audio endpoint unavailable".into());
        };
        if gain {
            let v = ctl.nudge(arg.as_f64().unwrap_or(0.0) as f32 / 100.0);
            return (true, format!("{} vol -> {}%", ep.name, (v * 100.0).round() as i32));
        }
        let s = match arg.as_str().unwrap_or("toggle") {
            "on" => {
                ctl.set_mute(true);
                true
            }
            "off" => {
                ctl.set_mute(false);
                false
            }
            _ => ctl.toggle_mute(),
        };
        return (true, format!("{} mute -> {}", ep.name, if s { "ON" } else { "off" }));
    }

    // ── BRIGHTNESS: lighting brightness write ────────────────────────────────────────────────────
    if verb == "brightness" {
        let Some(pct) = arg.as_u64() else {
            return (false, "brightness(pct): pct must be a number".into());
        };
        let pct = pct.min(100) as u8;
        return match open_capable(crate::registry::Capability::SetBrightness) {
            Some(d) => match cap::set_brightness(&d, pct, cap::Store::Persist) {
                Ok(()) => (true, format!("brightness -> {pct}%")),
                Err(e) => (false, format!("brightness failed: {e}")),
            },
            None => (false, "no brightness-capable device".into()),
        };
    }

    // ── SIGNAL: drive the lighting engine's macro channels (the `signal` DATA layer) ─────────────
    // `signal(channel, value)` — channel 1-indexed, value clamped 0..=1 at the engine. Ungated like
    // `store`: the value is process-state for the compositor (what a painted Signal layer renders);
    // actual device writes stay behind the writes gate downstream, so SAFE mode still holds.
    if verb == "signal" {
        let ch = arg.get("ch").and_then(Value::as_u64);
        let v = arg.get("value").and_then(Value::as_f64);
        let (Some(ch), Some(v)) = (ch, v) else {
            return (false, "signal(channel, value): channel 1-4, value 0..1".into());
        };
        if ch == 0 || ch as usize > crate::lighting::SIGNAL_CHANNELS {
            return (
                false,
                format!("signal: channel must be 1-{}", crate::lighting::SIGNAL_CHANNELS),
            );
        }
        crate::lighting::set_signal(ch as usize - 1, v as f32);
        return (true, format!("signal {ch} -> {:.2}", v.clamp(0.0, 1.0)));
    }

    // ── OBS (obs-websocket, via the app-installed sink) ──────────────────────────────────────────
    // Control lives in the app (it owns the protocol host); core routes the verb through the sink
    // the app installs when CONNECTIONS + OBS are on. Unclaimed when off, so it fails honestly
    // rather than pretending. WRITE: `obs_scene(name)` / `obs_stream(["toggle"|"start"|"stop"])` /
    // `obs_record([..|"pause"])` / `obs_replay(["save"|"start"|"stop"])` /
    // `obs_mute("Mic/Aux")` / `obs_request(type[, data])` (the whole
    // obs-websocket API). SENSE: `obs_get("scene"|"streaming"|"recording"|"connected")` reads the
    // app-side mirror of OBS's own events. And the flow runs BOTH ways: the app fires the
    // `on_obs_scene` / `on_obs_stream` / `on_obs_record` hook macros when OBS itself changes.
    if verb.starts_with("obs_") {
        return match crate::obs_hook::dispatch(verb, arg) {
            Some(r) => r,
            None => (false, "OBS not connected (open SYSTEM → CONNECTIONS)".into()),
        };
    }

    // ── device + profile INTENTS (shared path — the same write a bound trigger uses) ─────────────
    let dir = if arg.as_str() == Some("down") || arg.as_i64() == Some(-1) {
        Direction::Down
    } else {
        Direction::Up
    };
    let intent = match verb {
        "dpi" => match arg.as_u64() {
            Some(n) => Intent::DpiSet(n.clamp(100, 30_000) as u16),
            None => return (false, "dpi(n): n must be a number".into()),
        },
        "dpi_cycle" => Intent::DpiCycle(dir),
        "scroll_cycle" => Intent::ScrollStageCycle(dir),
        "profile" => match arg.as_str() {
            Some(s) if !s.is_empty() => Intent::ProfileSwitch(s.to_string()),
            _ => return (false, "profile(name): name must be a non-empty string".into()),
        },
        "profile_cycle" => Intent::ProfileCycle(dir),
        other => return (false, format!("unknown device verb '{other}'")),
    };
    let Some(reg) = act_registry() else {
        return (false, "device registry failed to load".into());
    };
    let mut devices = crate::device::DeviceSession::new(reg);
    let mut cursor = crate::intent::ProcessProfileCursor;
    match crate::intent::run_shared_intent(
        &mut devices,
        &mut cursor,
        &intent,
        crate::dpi_origin::Cause::UserApplied,
    ) {
        Some(msg) => (true, msg),
        None => (false, "that action isn't available to macros".into()),
    }
}

/// Read protocol frames off the sidecar's stdout and route them. On EOF/error -> mark the link
/// dead (which wakes `ensure`'s warm-wait and fails every in-flight waiter). `stdin` is the weak
/// write-end used ONLY for the no-UI prompt auto-answer (so an asking macro is never stranded).
fn reader_loop(
    stdout: std::process::ChildStdout,
    shared: Arc<Shared>,
    beacon: BeaconSlot,
    stdin: Weak<Mutex<ChildStdin>>,
    mode: MacroMode,
) {
    let mut r = BufReader::new(stdout);
    loop {
        let mut len = [0u8; 4];
        if r.read_exact(&mut len).is_err() {
            break;
        }
        let n = u32::from_le_bytes(len) as usize;
        if n > (16 << 20) {
            break; // desync guard
        }
        let mut body = vec![0u8; n];
        if r.read_exact(&mut body).is_err() {
            break;
        }
        // A frame whose length prefix was valid but whose body isn't JSON means the byte stream
        // has desynced — every subsequent length prefix would be garbage too. Bail and respawn.
        let Ok(v) = serde_json::from_slice::<Value>(&body) else {
            break;
        };
        crate::prof::bump(&crate::prof::READER_FRAME);
        match v.get("t").and_then(Value::as_str) {
            Some("ready") => {
                let expected = match mode {
                    MacroMode::Raw => "raw",
                    MacroMode::Bound => "bound",
                };
                if v.get("mode").and_then(Value::as_str) != Some(expected) {
                    shared.push_log(format!(
                        "[macro host] refused mismatched sidecar: expected {expected}, got {:?}",
                        v.get("mode")
                    ));
                    break;
                }
                shared.push_log(format!("[macro host] {expected} ready"));
                let mut st = shared.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                st.warm = true;
                st.dead = false;
                shared.cv.notify_all();
            }
            Some("result") | Some("checked") | Some("pong") | Some("registered")
            | Some("prepared") | Some("committed") | Some("parsed") => {
                // Hand the frame to its waiter if one is registered. A `result` with NO waiter —
                // whether the rid is null (a fire-and-forget `invoke(wait=False)` child, dispatched
                // with `rid: None`) OR a numeric rid nobody is waiting on (a top-level `fire_async`,
                // or an `invoke` that already timed out) — still carries the macro's stdout/traceback,
                // so it must be surfaced to the macro log, never dropped. (Before, a null rid
                // short-circuited this whole arm, so an async-INVOKED child that crashed failed
                // INVISIBLY — its traceback reached no one.)
                let waiter = v
                    .get("rid")
                    .and_then(Value::as_u64)
                    .and_then(|rid| shared.pending.lock().unwrap_or_else(std::sync::PoisonError::into_inner).remove(&rid));
                if let Some(tx) = waiter {
                    let _ = tx.send(v.clone());
                } else if v.get("t").and_then(Value::as_str) == Some("result") {
                    // an async fire result (no waiter) -> surface to the log.
                    if v.get("ok").and_then(Value::as_bool) == Some(false) {
                        // the FULL traceback, every line — a crash must stay debuggable, not be
                        // reduced to its "Traceback (most recent call last):" banner (the real
                        // exception is on the LAST line).
                        if let Some(e) = v.get("error").and_then(Value::as_str) {
                            let mut lines = e.lines();
                            if let Some(first) = lines.next() {
                                shared.push_log(format!("[macro error] {first}"));
                            }
                            for line in lines {
                                shared.push_log(line.to_string());
                            }
                        }
                    } else if let Some(l) = v.get("log").and_then(Value::as_str) {
                        for line in l.lines() {
                            shared.push_log(line.to_string());
                        }
                    }
                }
            }
            Some("prompt") => {
                // a macro called neuron.ask(...) — route to the UI, or auto-dismiss honestly.
                let pid = v.get("pid").and_then(Value::as_u64).unwrap_or(0);
                let macro_id = v
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or("?")
                    .to_string();
                let text = v
                    .get("text")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let detail = v
                    .get("detail")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                // the answer wheel's wedges — the macro's option labels. Absent/empty (a bare ask)
                // falls back to yes/no, so the simplest prompt needs nothing extra on the wire.
                let options: Vec<String> = v
                    .get("options")
                    .and_then(Value::as_array)
                    .map(|a| a.iter().filter_map(|o| o.as_str().map(str::to_string)).collect())
                    .filter(|v: &Vec<String>| !v.is_empty())
                    .unwrap_or_else(|| vec!["yes".into(), "no".into()]);
                let timeout_ms = v
                    .get("timeout")
                    .and_then(Value::as_f64)
                    .map(|s| (s * 1000.0).max(0.0) as u64);
                let taken = beacon_deliver(
                    &beacon,
                    BeaconEvent::Ask {
                        pid,
                        macro_id: macro_id.clone(),
                        text: text.clone(),
                        options,
                        detail,
                        timeout_ms,
                    },
                );
                if !taken {
                    // no UI listening: dismiss NOW so the macro's ask returns its default instead
                    // of hanging until timeout. The auto-answer goes straight down the pipe.
                    if let Some(stdin) = stdin.upgrade() {
                        let _ = send_frame(
                            &mut *stdin.lock().unwrap_or_else(std::sync::PoisonError::into_inner),
                            &json!({"t": "answer", "pid": pid, "choice": Value::Null}),
                        );
                    }
                    shared.push_log(format!(
                        "[beacon] '{macro_id}' asked \"{text}\" — no UI attached, dismissed"
                    ));
                }
            }
            Some("prompt_done") => {
                // sidecar-side timeout — withdraw the prompt from any UI still showing it.
                if let Some(pid) = v.get("pid").and_then(Value::as_u64) {
                    beacon_deliver(&beacon, BeaconEvent::Retire { pid });
                }
            }
            Some("notify") => {
                let macro_id = v
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or("?")
                    .to_string();
                let text = v
                    .get("text")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                shared.push_log(format!("[{macro_id}] {text}"));
                beacon_deliver(&beacon, BeaconEvent::Notify { macro_id, text });
            }
            Some("act") => {
                // a macro called a device verb (neuron.dpi/profile/…). Run the SHARED intent — the
                // exact write (+ confirmation card) a bound trigger uses — OFF the reader thread so a
                // device round-trip never stalls result/answer routing, then hand the macro back the
                // outcome it's blocking on.
                let rid = v.get("rid").and_then(Value::as_u64);
                let verb = v
                    .get("verb")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let arg = v.get("arg").cloned().unwrap_or(Value::Null);
                let macro_id = v.get("id").and_then(Value::as_str).unwrap_or("?").to_string();
                let mock = v.get("mock").and_then(Value::as_bool).unwrap_or(false);
                let stdin = stdin.clone();
                // The `act_result` frame is MANDATORY — the macro blocks on it. The worker only
                // RUNS the verb and returns its outcome; `done` sends the frame exactly once,
                // whether the verb completed, panicked, or the thread was refused (synthesizing a
                // failure result in the latter cases) — so the macro can never hang waiting.
                crate::worker::spawn_notify(
                    "macro-host-act",
                    move || run_act(mode, &macro_id, &verb, &arg, mock),
                    move |outcome| {
                        let (ok, msg) = outcome.unwrap_or_else(|| {
                            (false, "act worker could not run (thread refused or panicked)".into())
                        });
                        if let Some(stdin) = stdin.upgrade() {
                            let _ = send_frame(
                                &mut *stdin
                                    .lock()
                                    .unwrap_or_else(std::sync::PoisonError::into_inner),
                                &json!({"t": "act_result", "rid": rid, "ok": ok, "msg": msg}),
                            );
                        }
                    },
                );
            }
            _ => {}
        }
    }
    // every open prompt died with this sidecar — clear any UI queue before reporting the death.
    beacon_deliver(&beacon, BeaconEvent::RetireAll);
    shared.mark_dead();
}

/// Build a [`ParseError::Host`] from any displayable message (the non-syntax failure path).
fn host_err(msg: impl std::fmt::Display) -> ParseError {
    ParseError::Host {
        msg: msg.to_string(),
    }
}

/// Marshal the captured Context (+ live arm state) into the json the host's `Ctx` reads.
fn ctx_json(ctx: &crate::macros::context::Context, armed: bool) -> Value {
    json!({
        "app": ctx.app(),
        "title": ctx.title(),
        "cwd": ctx.cwd().map(|p| p.to_string_lossy().into_owned()),
        "clipboard": ctx.clipboard(),
        "selection": ctx.selection(),
        "prev_window": ctx.prev_window().0,
        "armed": armed,
    })
}

// ── runtime resolution (bundled CPython; operator override) ─────────────────────────────────────

/// Resolve the [`Runtime`] the sidecar spawns from. Two tiers, in order:
///   1. **`NEURON_PYTHON`** — an explicit interpreter path the operator names (a venv/pyenv they
///      WANT used). Still honoured for power users; the host scripts are taken from the bundled,
///      app-materialized `host/` dir either way (so the protocol scripts are always the right ones).
///   2. **the BUNDLED runtime** — the interpreter `neuron` ships in its own binary and materializes
///      into the user's data dir ([`crate::macros::ensure_runtime`]). The default ship path: a
///      known-good CPython, zero user setup, no system-PATH probing, no env-var hacks.
///
/// There is NO system-PATH discovery: `neuron` carries its own Python, so the macro tier never
/// depends on what (if anything) the user has installed. An `Err` here means a real IO failure
/// materializing the bundle, surfaced verbatim — never a silent fallback.
fn resolve_runtime() -> Result<crate::macros::Runtime, String> {
    let mut rt = crate::macros::ensure_runtime()?;
    // Operator override: an explicit interpreter wins, but it drives the SAME bundled host scripts.
    if let Ok(p) = std::env::var("NEURON_PYTHON") {
        let pb = PathBuf::from(p);
        if pb.exists() {
            rt.python = pb;
        }
    }
    Ok(rt)
}

// ── macro persistence (macros/scripts/<id>.py) ──────────────────────────────────────────────────

/// Directory holding python macro sources (in the run root, like the rest of Neuron's config).
pub fn macros_dir() -> PathBuf {
    crate::runroot::run_root().join("macros").join("scripts")
}

const BOUND_DEFAULT_MIGRATION: &str = ".bound_default_v1";

/// Preserve the pre-policy contract exactly once. Every Python file already present when this build
/// first observes the macro directory came from the unrestricted era, so stamp it RAW atomically.
/// The marker is written last; interruption retries idempotently instead of silently downgrading old
/// code into the new BOUND default.
fn migrate_legacy_macro_dir(dir: &std::path::Path) -> Result<(), String> {
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    let marker = dir.join(BOUND_DEFAULT_MIGRATION);
    if marker.exists() {
        return Ok(());
    }
    let rd = std::fs::read_dir(dir).map_err(|e| e.to_string())?;
    for entry in rd.flatten() {
        let path = entry.path();
        if path.extension().and_then(|x| x.to_str()) != Some("py") {
            continue;
        }
        let src = std::fs::read_to_string(&path).map_err(|e| e.to_string())?;
        let raw = match crate::macros::mode_from_source(&src) {
            Ok(crate::macros::MacroMode::Raw) => src,
            Ok(crate::macros::MacroMode::Bound) => {
                crate::macros::set_source_mode(&src, crate::macros::MacroMode::Raw)
            }
            Err(e) => return Err(format!("{}: {e}", path.display())),
        };
        if raw != src {
            crate::salvage::atomic_write(&path, raw.as_bytes()).map_err(|e| e.to_string())?;
        }
    }
    crate::salvage::atomic_write(
        &marker,
        b"pre-BOUND macros were stamped '# neuron: raw' once; new macros default BOUND.\n",
    )
    .map_err(|e| e.to_string())
}

fn ensure_macro_policy_migration() -> Result<(), String> {
    migrate_legacy_macro_dir(&macros_dir())
}

/// Exemplar macros shipped with the binary (id, source). Written to disk on a truly-fresh install
/// so a new user has a working `neuron.ask` macro to read, run, and copy from.
pub const DEFAULT_MACROS: &[(&str, &str)] =
    &[("beacon_demo", include_str!("defaults/beacon_demo.py"))];

/// Seed the bundled exemplar macros into THIS install's macros dir, once.
///
/// Gated on a one-time MARKER file (`.defaults_seeded`), NOT on the dir's existence — the macros
/// dir can already exist (empty, or holding the user's own macros) and we still want a fresh
/// install to receive the bundled exemplar(s). The marker is what makes a DELETE stick: once we've
/// seeded, the marker is present, so a user who removes the exemplar (SYSTEM panel's delete control)
/// never sees it resurrect. Delete the marker to opt back into re-seeding.
///
/// Best-effort: IO errors are swallowed, matching the rest of this module's persistence.
pub fn seed_default_macros() {
    let dir = macros_dir();
    // Run the authority migration BEFORE writing bundled examples. Existing user files therefore
    // retain RAW, while examples created by this build are genuinely new and stay BOUND.
    if ensure_macro_policy_migration().is_err() {
        return;
    }
    let marker = dir.join(".defaults_seeded");
    if marker.exists() {
        return;
    }
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    for (id, src) in DEFAULT_MACROS {
        let _ = std::fs::write(dir.join(format!("{id}.py")), src);
    }
    let _ = std::fs::write(
        &marker,
        b"neuron seeded its bundled default macros here once. delete this file to re-seed.\n",
    );
}

fn macro_path(id: &str) -> PathBuf {
    macros_dir().join(format!("{}.py", sanitize_id(id)))
}

/// Keep an id filesystem-safe (it is a file stem + a registry key). Conservative allowlist.
fn sanitize_id(id: &str) -> String {
    id.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

fn validate_macro_id(id: &str) -> Result<(), String> {
    if id.is_empty() || sanitize_id(id) != id {
        Err("macro name may contain only ASCII letters, digits, '_' and '-'".into())
    } else {
        Ok(())
    }
}

fn write_macro_file(id: &str, source: &str) -> Result<(), String> {
    let dir = macros_dir();
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    crate::salvage::atomic_write(&macro_path(id), source.as_bytes()).map_err(|e| e.to_string())
}

// ── self-describing-option VALUES: chosen in the GUI (or hand-edited), kept beside the scripts ──
fn options_dir() -> PathBuf {
    crate::runroot::run_root().join("macros").join("options")
}

fn options_path(id: &str) -> PathBuf {
    options_dir().join(format!("{}.json", sanitize_id(id)))
}

/// The user's chosen option values for `id` (a `{key: value}` object). `{}` if none on disk.
/// Parsed option values, cached in memory per macro id.
///
/// This exists because [`load_option_values`] is called from `fire_dispatch` — the LIVE DISPATCH
/// PATH — and its original form did a synchronous `read_to_string` + JSON parse on the input thread
/// for **every single macro press**. A file read is microseconds when the page cache is warm and
/// milliseconds when it is not (first press after boot, after an antivirus scan, on a slow volume),
/// which put an unbounded, invisible disk dependency directly in front of the user's keypress.
///
/// Cached values are invalidated by [`invalidate_option_cache`], which every writer calls. That means
/// a hand-edit of the JSON outside the app is picked up on the next macro (re)register or reload
/// rather than instantly — the same contract as every other config this app loads, and the right
/// trade for taking the disk off the input path.
fn option_cache() -> &'static Mutex<HashMap<String, Value>> {
    static CACHE: OnceLock<Mutex<HashMap<String, Value>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn load_option_values(id: &str) -> Value {
    // A leaf lock: nothing else is acquired while it is held, so it cannot participate in a deadlock
    // even though `fire_dispatch` calls this while holding the host's own lock.
    {
        let cache = option_cache().lock().unwrap_or_else(|e| e.into_inner());
        if let Some(v) = cache.get(id) {
            return v.clone();
        }
    }
    // The lock is NOT held across this read — see `cache_if_absent` for why that is safe.
    let from_disk = read_option_values_from_disk(id);
    cache_if_absent(id, from_disk)
}

/// Cache `value` for `id` only if nothing is cached yet, and return whatever ends up cached.
///
/// The "only if absent" is the whole point. `load_option_values` deliberately drops the cache lock
/// while it reads the file (holding a mutex across disk I/O on the live dispatch path is worse than
/// the problem it would solve), which opens a window: the UI can save NEW options and cache them
/// while a reader is still mid-read of the OLD file. An unconditional insert then clobbers the newer
/// value with the older one, and every later macro press uses stale options despite a successful
/// save — with nothing to correct it until the next reload.
///
/// Inserting only when vacant makes the writer always win, and returns the fresher value to the
/// reader as a bonus.
fn cache_if_absent(id: &str, value: Value) -> Value {
    option_cache()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .entry(id.to_string())
        .or_insert(value)
        .clone()
}

/// The uncached read — the only place that touches the options file for reading.
fn read_option_values_from_disk(id: &str) -> Value {
    std::fs::read_to_string(options_path(id))
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .filter(Value::is_object)
        .unwrap_or_else(|| json!({}))
}

/// Drop `id`'s cached options so the next read comes from disk. Called by every writer, and on
/// (re)register so a reload picks up externally-edited values.
fn invalidate_option_cache(id: &str) {
    option_cache()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(id);
}

fn write_option_values(id: &str, values: &Value) -> Result<(), String> {
    std::fs::create_dir_all(options_dir()).map_err(|e| e.to_string())?;
    let body = serde_json::to_string_pretty(values).map_err(|e| e.to_string())?;
    crate::salvage::atomic_write(&options_path(id), body.as_bytes()).map_err(|e| e.to_string())?;
    // Cache the value we just wrote rather than merely dropping it: the UI's save is immediately
    // followed by the user testing the macro, and that press should not have to go to disk either.
    option_cache()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(id.to_string(), values.clone());
    Ok(())
}

#[cfg(test)]
mod option_cache_tests {
    use super::*;

    /// The point of the cache: once an id is cached, reads come from memory and NOT from the disk.
    /// Proven without touching the filesystem — a sentinel is placed in the cache that no file could
    /// have produced, so seeing it back means the disk path was skipped. That is the whole property,
    /// since the reason this exists is to keep a file read off the live keypress path.
    #[test]
    fn a_cached_read_does_not_go_to_disk() {
        let id = "neuron_test_option_cache_hit";
        invalidate_option_cache(id);
        assert_eq!(
            load_option_values(id),
            json!({}),
            "an unknown macro's options read as an empty object"
        );
        // A value no options file contains. If `load_option_values` consulted the disk it would come
        // back as `{}` again and this would fail.
        option_cache()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(id.to_string(), json!({"sentinel": 42}));
        assert_eq!(
            load_option_values(id),
            json!({"sentinel": 42}),
            "the cached value was returned, so the fire path did no file I/O"
        );
        invalidate_option_cache(id);
        assert_eq!(
            load_option_values(id),
            json!({}),
            "invalidation sends the next read back to disk"
        );
    }

    /// A reader that lost the race must not overwrite a fresher save.
    ///
    /// The real interleaving is: a macro fire misses the cache and starts reading the OLD file; the UI
    /// saves NEW options and caches them; the reader finishes and inserts what it read. If that insert
    /// won, the save would be silently undone in memory and every later press would use stale options
    /// while the file on disk said otherwise — the worst kind of bug, because the UI shows success.
    ///
    /// Pinned deterministically on `cache_if_absent` (the exact step that races) rather than by
    /// spawning threads and hoping the window is hit, which would be flaky and prove less.
    #[test]
    fn a_slow_reader_cannot_clobber_a_newer_save() {
        let id = "neuron_test_option_cache_race";
        invalidate_option_cache(id);
        // The UI's save lands first.
        option_cache()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(id.to_string(), json!({"v": "new"}));
        // The slow reader now finishes and tries to cache the value it read BEFORE that save.
        let got = cache_if_absent(id, json!({"v": "old"}));
        assert_eq!(
            got,
            json!({"v": "new"}),
            "the reader was handed the stale value it read instead of the fresher cached one"
        );
        assert_eq!(
            load_option_values(id),
            json!({"v": "new"}),
            "a stale reader overwrote a newer save — the saved options would be silently ignored"
        );
        invalidate_option_cache(id);
    }

    #[test]
    fn caching_is_per_macro_id() {
        // A shared cache keyed wrongly would hand one macro another's options — a correctness bug far
        // worse than the latency it was added to fix, so pin the isolation.
        let (a, b) = ("neuron_test_opt_a", "neuron_test_opt_b");
        invalidate_option_cache(a);
        invalidate_option_cache(b);
        option_cache()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(a.to_string(), json!({"who": "a"}));
        assert_eq!(load_option_values(a), json!({"who": "a"}));
        assert_eq!(load_option_values(b), json!({}), "b did not inherit a's options");
        invalidate_option_cache(a);
        invalidate_option_cache(b);
    }
}

/// On (re)register, fill in any option the user hasn't set yet with its declared default — so a
/// freshly-added plugin runs with sensible values and never sees a missing key.
fn seed_option_defaults(id: &str, manifest: &Value) {
    let Some(opts) = manifest.as_array() else {
        return;
    };
    // (Re)register is the reload boundary, so re-read from disk here rather than trusting the cache —
    // it is how a hand-edited options file gets picked up (see `option_cache`).
    invalidate_option_cache(id);
    let mut values = load_option_values(id);
    let map = match values.as_object_mut() {
        Some(m) => m,
        None => {
            values = json!({});
            values.as_object_mut().unwrap()
        }
    };
    let mut changed = false;
    for o in opts {
        let Some(key) = o.get("key").and_then(Value::as_str) else {
            continue;
        };
        if !map.contains_key(key) {
            map.insert(
                key.to_string(),
                o.get("default").cloned().unwrap_or(Value::Null),
            );
            changed = true;
        }
    }
    if changed {
        let _ = write_option_values(id, &values);
    }
}

/// Scan macros/scripts/*.py into (id, source) pairs (id = file stem).
pub fn scan_macro_dir() -> Vec<(String, String)> {
    if let Err(e) = ensure_macro_policy_migration() {
        eprintln!("[macro] legacy authority migration failed: {e}");
        return Vec::new();
    }
    let mut out = Vec::new();
    if let Ok(rd) = std::fs::read_dir(macros_dir()) {
        for e in rd.flatten() {
            let p = e.path();
            if p.extension().and_then(|x| x.to_str()) == Some("py") {
                if let (Some(stem), Ok(src)) = (
                    p.file_stem().and_then(|s| s.to_str()),
                    std::fs::read_to_string(&p),
                ) {
                    if validate_macro_id(stem).is_ok() {
                        out.push((stem.to_string(), src));
                    }
                }
            }
        }
    }
    out
}

/// The names of all macros currently on disk (for the GUI/CLI list).
pub fn list_macros() -> Vec<String> {
    let mut v: Vec<String> = scan_macro_dir().into_iter().map(|(id, _)| id).collect();
    v.sort();
    v
}

/// Load a macro's source from disk by id.
pub fn load_macro(id: &str) -> Option<String> {
    validate_macro_id(id).ok()?;
    ensure_macro_policy_migration().ok()?;
    std::fs::read_to_string(macro_path(id)).ok()
}

/// Free-function convenience over [`MacroHost::parse_macro`] on the process-global host (mirrors how
/// `register`/`scan_macro_dir` are reachable both as methods and module functions). Parses macro
/// `source` into the typed [`MacroNode`] tree; pair with [`crate::macros::nodes_to_source`] for the
/// inverse. Do NOT call from the input/UI thread (it can block up to [`FIRE_BUDGET`]).
pub fn parse_document(source: &str) -> DocumentParseResult {
    macro_host().parse_document(source)
}

pub fn parse_macro(source: &str) -> ParseResult {
    macro_host().parse_macro(source)
}

/// Delete a macro from disk and from the warm/durable registry immediately.
pub fn delete_macro(id: &str) -> std::io::Result<()> {
    macro_host()
        .delete(id)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))
}

/// Serializes every test that exclusively drives — or outright KILLS — the process-global
/// `macro_host()` sidecar singleton. The sidecar and its `pending` map are shared by all tests in
/// this binary; a test that taskkills the child (the death-race tests) or does many real
/// round-trips (`node`'s codegen proptest) would otherwise corrupt a concurrent sidecar test's
/// `pending` view or answer stream. Anyone touching the live sidecar in anger holds this first.
/// Poison-tolerant: a panicking holder must not wedge every later sidecar test.
#[cfg(test)]
pub(crate) static SIDECAR_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_id_is_fs_safe() {
        assert_eq!(sanitize_id("ok_name-1"), "ok_name-1");
        assert_eq!(sanitize_id("../etc/passwd"), "___etc_passwd");
        assert_eq!(sanitize_id("a b.c"), "a_b_c");
    }

    #[test]
    fn legacy_macro_migration_is_one_shot_and_new_files_stay_bound() {
        let dir = std::env::temp_dir().join(format!(
            "neuron_macro_policy_migration_{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let old = dir.join("old.py");
        std::fs::write(&old, "def macro(ctx):\n    return 1\n").unwrap();

        migrate_legacy_macro_dir(&dir).unwrap();
        let migrated = std::fs::read_to_string(&old).unwrap();
        assert_eq!(
            crate::macros::mode_from_source(&migrated).unwrap(),
            crate::macros::MacroMode::Raw
        );

        let fresh = dir.join("fresh.py");
        std::fs::write(&fresh, "def macro(ctx):\n    return 2\n").unwrap();
        migrate_legacy_macro_dir(&dir).unwrap();
        let fresh_src = std::fs::read_to_string(&fresh).unwrap();
        assert_eq!(
            crate::macros::mode_from_source(&fresh_src).unwrap(),
            crate::macros::MacroMode::Bound
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn macro_ids_are_canonical_instead_of_lossily_aliased() {
        assert!(validate_macro_id("ok_name-1").is_ok());
        for bad in ["", "a b", "a/b", "a:b", "../same"] {
            assert!(validate_macro_id(bad).is_err(), "{bad:?} must not become a different file key");
        }
    }

    #[test]
    fn directory_scan_ignores_noncanonical_python_stems() {
        let _env = crate::runroot::ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let tmp = std::env::temp_dir().join(format!(
            "neuron_macro_scan_identity_{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        {
            let _pin = crate::runroot::RunDirPin::to(&tmp);
            let dir = macros_dir();
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("good_id.py"), "def macro(ctx):\n    pass\n").unwrap();
            std::fs::write(dir.join("bad name.py"), "def macro(ctx):\n    pass\n").unwrap();
            let found = scan_macro_dir();
            assert!(found.iter().any(|(id, _)| id == "good_id"));
            assert!(
                found.iter().all(|(id, _)| id != "bad name"),
                "a filename that cannot be a MacroId entered the runtime registry"
            );
        }
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn rust_refuses_effectful_act_frames_while_disarmed() {
        crate::action::arm_input(false);
        let (ok, msg) = run_act(
            MacroMode::Bound,
            "forged_bound",
            "clipboard_set",
            &json!("must-not-land"),
            false,
        );
        assert!(!ok, "forged effect frame bypassed the host authority check");
        assert_eq!(msg, "[disarmed]");
    }

    #[test]
    fn breaker_trips_after_repeated_crashes_then_resets() {
        let mut b = Breaker::default();
        for _ in 0..=BREAKER_MAX {
            b.record_crash();
        }
        assert!(b.tripped(), "breaker trips past the crash ceiling");
        b.reset();
        assert!(!b.tripped(), "reset clears the breaker");
    }

    #[test]
    fn bundled_runtime_materializes() {
        // The sidecar runs from the app-bundled CPython: resolving it must succeed (it extracts the
        // embedded interpreter on first call). This also proves availability no longer depends on a
        // system Python.
        let rt = resolve_runtime().expect("bundled runtime materializes");
        assert!(rt.python.exists(), "bundled python binary should exist");
        assert!(
            rt.host_script.ends_with("neuron_host.py"),
            "host entry script is neuron_host.py"
        );
        assert_eq!(
            rt.host_script.parent(),
            Some(rt.host_dir.as_path()),
            "host script must be co-located in host_dir (so `import neuron` resolves the sibling)"
        );
        assert!(
            rt.host_dir.join("neuron.py").exists(),
            "neuron.py must be co-located with neuron_host.py"
        );
    }

    #[test]
    fn macros_dir_is_under_macros() {
        assert!(macros_dir().ends_with("scripts"));
    }

    // ── death-race tests ──────────────────────────────────────────────────────────────────────
    //
    // These pin the known timeout-masked bug class documented at `Shared::mark_dead` and the
    // `pending_insert.before` site above: a request that inserts its `pending` waiter racing
    // mark_dead's clear can strand that waiter — bounded only by the caller's recv_timeout, never
    // truly leaked (the caller's own `Err(_)` arm removes it), but slow and silent instead of
    // failing fast. Both tests use the REAL bundled sidecar (same skip-cleanly-with-no-python
    // contract as `macro_host_respawn.rs`) and `crate::runroot::ENV_LOCK` to safely retarget
    // `NEURON_RUN_DIR` from a crate-internal unit test — these run in the SAME process as every
    // other lib unit test, including `macros::node`'s `codegen_parse_codegen_is_stable` proptest,
    // which also uses the process-global `macro_host()`. That sharing is a pre-existing property of
    // the singleton (not introduced here); what IS new is that these tests deliberately kill the
    // live sidecar process, which could in principle cause a transient, unrelated failure in
    // whatever other macro_host() consumer happens to be mid-request at that exact moment. The
    // window is small (kill + recover completes in well under a second) but it is a real,
    // non-zero risk worth flagging rather than hiding.
    //
    // Rather than simulate `mark_dead` by calling it directly (which would leave the REAL sidecar
    // process alive and still able to answer — defeating the point, since the actual bug requires
    // the reader thread to have permanently stopped routing replies), these kill the real process,
    // exactly like `macro_host_respawn.rs`, so the race is reproduced against real IPC.

    fn death_race_ctx() -> crate::macros::context::Context {
        crate::macros::context::Context::synthetic(Some("failpoint.exe".into()), None, None, None, None)
    }

    /// Shared setup for both death-race tests: isolate the run root, ensure a warm host with one
    /// registered echo macro, and return (host, ctx, tmp dir to clean up). Skips (returns `None`)
    /// when no bundled python runtime resolves — the same honest skip every other sidecar test uses.
    fn death_race_setup(tag: &str) -> Option<(&'static MacroHost, crate::macros::context::Context, PathBuf)> {
        let tmp = std::env::temp_dir().join(format!("neuron_failpoint_race_{tag}_{}", std::process::id()));
        std::fs::create_dir_all(&tmp).ok()?;
        let host = macro_host();
        if !host.available() {
            eprintln!("skipping macro_host death-race ({tag}): bundled python runtime did not materialize");
            let _ = std::fs::remove_dir_all(&tmp);
            return None;
        }
        host.set_armed(false); // the test macro is read-only; no input synthesis needed
        let src = "def macro(ctx):\n    return 'ok'\n";
        if let Err(e) = host.register("fp_death_race", src) {
            eprintln!("skipping macro_host death-race ({tag}): register failed: {e}");
            let _ = std::fs::remove_dir_all(&tmp);
            return None;
        }
        Some((host, death_race_ctx(), tmp))
    }

    #[cfg(windows)]
    fn kill_pid(pid: u32) {
        let _ = std::process::Command::new("taskkill")
            .args(["/F", "/PID", &pid.to_string()])
            .status();
    }
    #[cfg(not(windows))]
    fn kill_pid(pid: u32) {
        let _ = std::process::Command::new("kill")
            .args(["-9", &pid.to_string()])
            .status();
    }

    /// Race (b): the request's insert lands AFTER mark_dead's clear (freeze the REQUEST side).
    /// Proves: the request still resolves within its budget (never hangs past it), the pending map
    /// is empty afterward (no permanent leak — only the documented timeout-bounded delay), and a
    /// subsequent request against the auto-respawned session succeeds.
    #[test]
    #[ignore = "kills the real sidecar process + needs the bundled Python runtime; opt in with --ignored (also mutates the global macro-host singleton, so it is not a fast-suite test)"]
    fn death_race_insert_after_clear_is_bounded_and_recovers() {
        let _sidecar = SIDECAR_TEST_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let _fp = crate::failpoint::FAILPOINT_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let _env = crate::runroot::ENV_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some((host, ctx, tmp)) = death_race_setup("insert_after_clear") else { return };
        let _pin = crate::runroot::RunDirPin::to(&tmp);

        let baseline = host.invoke("fp_death_race", &ctx);
        assert!(baseline.contains("ok"), "baseline invoke failed: {baseline}");
        let pid_before = crate::prof::SIDECAR_PID.load(Ordering::Relaxed);

        // Freeze the NEXT insert-before-send window so mark_dead's clear (triggered by the kill
        // below) lands first, and the request's own insert follows it.
        let _armed = crate::failpoint::Armed::new(
            "macro_host.pending_insert.before",
            crate::failpoint::Action::Sleep(Duration::from_millis(350)),
        );

        let (tx, rx) = channel::<String>();
        let t0 = Instant::now();
        std::thread::spawn(move || {
            let host = macro_host();
            let ctx = death_race_ctx();
            let r = host.invoke_with_budget("fp_death_race", &ctx, Duration::from_secs(2));
            let _ = tx.send(r);
        });

        // give the request thread time to pass ensure_locked and hit the armed failpoint before we
        // pull the sidecar out from under it.
        std::thread::sleep(Duration::from_millis(80));
        kill_pid(pid_before);

        let result = rx
            .recv_timeout(Duration::from_secs(3))
            .unwrap_or_else(|_| panic!("request did not return within its budget — a waiter leaked"));
        eprintln!("death-race(insert-after-clear) result in {:?}: {result}", t0.elapsed());

        assert!(
            crate::failpoint::hits("macro_host.pending_insert.before") > 0,
            "anti-vacuity: the armed failpoint was never reached"
        );

        // no waiter left behind in whatever session is now live.
        {
            let g = host.inner.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(s) = g.session.as_ref() {
                let pending = s.shared.pending.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                assert!(
                    pending.is_empty(),
                    "a waiter leaked into the live session's pending map: {:?}",
                    pending.keys().collect::<Vec<_>>()
                );
            }
        }

        // recovery: the next request must succeed against a freshly respawned sidecar.
        let healed = host.invoke("fp_death_race", &ctx);
        assert!(healed.contains("ok"), "post-race recovery failed: {healed}");
        let pid_after = crate::prof::SIDECAR_PID.load(Ordering::Relaxed);
        // A live sidecar answering "ok" above already proves recovery respawned the child;
        // do NOT assert the PID changed — Windows can legitimately reuse the just-freed PID
        // for the respawn, which is not a recovery failure. Keep the read for the log only.
        let _ = (pid_before, pid_after);
        assert!(
            crate::failpoint::hits("macro_host.ensure_locked.before_replace") > 0,
            "anti-vacuity: recovery must actually pass through session replacement"
        );

        host.unregister("fp_death_race");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// Race (c): mark_dead's clear lands AFTER a concurrent request's insert (freeze the DEATH
    /// side) — the opposite interleave of the test above, exercising `mark_dead.before_clear`
    /// instead of `pending_insert.before`. Same invariants: bounded, no leaked waiter, recovers.
    #[test]
    #[ignore = "kills the real sidecar process + needs the bundled Python runtime; opt in with --ignored (also mutates the global macro-host singleton, so it is not a fast-suite test)"]
    fn death_race_clear_after_insert_is_bounded_and_recovers() {
        let _sidecar = SIDECAR_TEST_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let _fp = crate::failpoint::FAILPOINT_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let _env = crate::runroot::ENV_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some((host, ctx, tmp)) = death_race_setup("clear_after_insert") else { return };
        let _pin = crate::runroot::RunDirPin::to(&tmp);

        let baseline = host.invoke("fp_death_race", &ctx);
        assert!(baseline.contains("ok"), "baseline invoke failed: {baseline}");
        let pid_before = crate::prof::SIDECAR_PID.load(Ordering::Relaxed);

        // Freeze mark_dead itself (the reader thread, on EOF) right before it clears `pending`, so
        // a concurrent request's insert can land first.
        let _armed = crate::failpoint::Armed::new(
            "macro_host.mark_dead.before_clear",
            crate::failpoint::Action::Sleep(Duration::from_millis(350)),
        );

        kill_pid(pid_before);
        // give the reader thread time to notice EOF and hit the armed failpoint (frozen there)
        // before the request below races its own insert in underneath it.
        std::thread::sleep(Duration::from_millis(80));

        let (tx, rx) = channel::<String>();
        let t0 = Instant::now();
        std::thread::spawn(move || {
            let host = macro_host();
            let ctx = death_race_ctx();
            let r = host.invoke_with_budget("fp_death_race", &ctx, Duration::from_secs(2));
            let _ = tx.send(r);
        });

        let result = rx
            .recv_timeout(Duration::from_secs(3))
            .unwrap_or_else(|_| panic!("request did not return within its budget — a waiter leaked"));
        eprintln!("death-race(clear-after-insert) result in {:?}: {result}", t0.elapsed());

        assert!(
            crate::failpoint::hits("macro_host.mark_dead.before_clear") > 0,
            "anti-vacuity: the armed failpoint was never reached"
        );

        {
            let g = host.inner.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(s) = g.session.as_ref() {
                let pending = s.shared.pending.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                assert!(
                    pending.is_empty(),
                    "a waiter leaked into the live session's pending map: {:?}",
                    pending.keys().collect::<Vec<_>>()
                );
            }
        }

        let healed = host.invoke("fp_death_race", &ctx);
        assert!(healed.contains("ok"), "post-race recovery failed: {healed}");
        let pid_after = crate::prof::SIDECAR_PID.load(Ordering::Relaxed);
        // A live sidecar answering "ok" above already proves recovery respawned the child;
        // do NOT assert the PID changed — Windows can legitimately reuse the just-freed PID
        // for the respawn, which is not a recovery failure. Keep the read for the log only.
        let _ = (pid_before, pid_after);

        host.unregister("fp_death_race");
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
