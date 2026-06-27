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

use crate::macros::node::MacroNode;
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

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

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
    /// The sidecar/runtime couldn't service the request (no python, pipe broken, did not answer).
    Host { msg: String },
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ParseError::Syntax { line, msg } => write!(f, "syntax error (line {line}): {msg}"),
            ParseError::Host { msg } => write!(f, "{msg}"),
        }
    }
}

impl std::error::Error for ParseError {}

/// The outcome of parsing a macro's Python source into the typed node tree: the macro body as an
/// ordered `Vec<MacroNode>` on success, or a [`ParseError`].
pub type ParseResult = Result<Vec<MacroNode>, ParseError>;

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
        let mut l = self.log.lock().unwrap();
        if l.len() >= LOG_RING {
            l.pop_front();
        }
        l.push_back(line);
    }
    fn mark_dead(&self) {
        {
            let mut s = self.state.lock().unwrap();
            s.dead = true;
            s.warm = false;
        }
        self.cv.notify_all();
        // fail every in-flight waiter so no blocking caller hangs past the sidecar's death.
        self.pending.lock().unwrap().clear();
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
        send_frame(&mut *self.stdin.lock().unwrap(), v)
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
    session: Option<Session>,
    /// id -> python source. The single source of truth for re-registration after a respawn.
    manifest: BTreeMap<String, String>,
    /// id -> the macro's DECLARED option manifest (its `NEURON_OPTIONS`, re-derived on register).
    /// The GUI renders controls from this; the user's chosen VALUES live on disk (`options_path`).
    options: BTreeMap<String, Value>,
    breaker: Breaker,
}

/// The process-global macro runtime.
pub struct MacroHost {
    inner: Mutex<Inner>,
    /// The SAFE/arm gate, kept OUTSIDE the inner lock so `set_armed` (called on the UI thread) is
    /// always lock-free and instant — it can never stall behind a cold-spawn warm-wait. The spawn
    /// reads this for the initial + warm-handshake arm state, so a respawn always reflects current.
    armed: AtomicBool,
    /// At most one background warm thread in flight (a burst of cold fires must not spawn one each).
    warm_in_flight: AtomicBool,
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
                manifest: BTreeMap::new(),
                options: BTreeMap::new(),
                breaker: Breaker::default(),
            }),
            armed: AtomicBool::new(false),
            warm_in_flight: AtomicBool::new(false),
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
        *self.beacon.lock().unwrap() = Some(tx);
        rx
    }

    /// Answer an open prompt: `Some(i)` = the user picked option `i`, `None` = passed/dismissed (the
    /// macro's prompt returns its `default`). An unknown/expired pid is ignored by the sidecar —
    /// answering late is always safe.
    pub fn answer(&self, pid: u64, choice: Option<usize>) {
        let mut g = self.inner.lock().unwrap();
        if let Some(s) = g.session.as_mut() {
            let _ = s.send(&json!({"t": "answer", "pid": pid, "choice": choice}));
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
            if let Some(s) = g.session.as_mut() {
                let _ = s.send(&json!({"t": "armed", "on": on}));
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

    /// Register (or replace) a macro by id with its python `source` and PERSIST it to disk. Blocks
    /// briefly for the sidecar's ack so a syntax error surfaces to the GUI/CLI. Safe to call from
    /// non-input threads (the GUI save, CLI add) — NOT the 1000 Hz path.
    pub fn register(&self, id: &str, source: &str) -> Result<(), String> {
        // persist first (the manifest mirrors disk; a respawn re-reads from here)
        write_macro_file(id, source)?;
        let rx = {
            let mut g = self.inner.lock().unwrap();
            g.manifest.insert(id.to_string(), source.to_string());
            self.ensure_locked(&mut g)?;
            let s = g.session.as_mut().unwrap();
            let rid = s.shared.next_rid.fetch_add(1, Ordering::Relaxed);
            let (tx, rx) = channel();
            s.shared.pending.lock().unwrap().insert(rid, tx);
            if !s.send(&json!({"t": "register", "rid": rid, "id": id, "source": source})) {
                return Err("sidecar pipe broken".into());
            }
            rx
        };
        // wait for the registered ack (bounded) outside the lock
        match rx.recv_timeout(FIRE_BUDGET) {
            Ok(v) if v.get("ok").and_then(Value::as_bool) == Some(true) => {
                // capture the macro's declared option manifest + seed any missing values to their
                // declared defaults, so a plugin works out of the box and the GUI has something
                // to render the moment it's added.
                let opts = v.get("options").cloned().unwrap_or(Value::Null);
                {
                    let mut g = self.inner.lock().unwrap();
                    if opts.is_array() {
                        g.options.insert(id.to_string(), opts.clone());
                    } else {
                        g.options.remove(id);
                    }
                }
                seed_option_defaults(id, &opts);
                Ok(())
            }
            Ok(v) => Err(v
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("register failed")
                .to_string()),
            Err(_) => Err("sidecar did not acknowledge (it may have crashed)".into()),
        }
    }

    /// A macro's DECLARED options (its `NEURON_OPTIONS` manifest) — what the GUI renders controls
    /// from. `None` (or empty) = the macro takes no options.
    pub fn options_manifest(&self, id: &str) -> Option<Value> {
        self.inner.lock().unwrap().options.get(id).cloned()
    }

    /// The user's chosen option VALUES for a macro (a `{key: value}` map), from disk. Empty if none.
    pub fn option_values(&self, id: &str) -> Value {
        load_option_values(id)
    }

    /// Persist the user's chosen option values for a macro (the GUI's save path).
    pub fn set_option_values(&self, id: &str, values: &Value) -> Result<(), String> {
        write_option_values(id, values)
    }

    /// Remove a macro (disk + manifest + sidecar registry).
    pub fn unregister(&self, id: &str) {
        let _ = std::fs::remove_file(macro_path(id));
        let mut g = self.inner.lock().unwrap();
        g.manifest.remove(id);
        g.options.remove(id);
        if let Some(s) = g.session.as_mut() {
            let _ = s.send(&json!({"t": "unregister", "id": id}));
        }
    }

    /// Syntax-check + list top-level defs WITHOUT executing (the honest "dry-run" — Python is
    /// full-power, so we never claim a behavioural trace). Returns the def names on success.
    pub fn check(&self, source: &str) -> Result<Vec<String>, String> {
        let (rx, shared, rid, rid_send) = {
            let mut g = self.inner.lock().unwrap();
            self.ensure_locked(&mut g)?;
            let s = g.session.as_mut().unwrap();
            let rid = s.shared.next_rid.fetch_add(1, Ordering::Relaxed);
            let (tx, rx) = channel();
            let shared = Arc::clone(&s.shared);
            shared.pending.lock().unwrap().insert(rid, tx);
            let ok = s.send(&json!({"t": "check", "rid": rid, "source": source}));
            (rx, shared, rid, ok)
        };
        if !rid_send {
            shared.pending.lock().unwrap().remove(&rid);
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
                shared.pending.lock().unwrap().remove(&rid);
                Err("sidecar did not answer".into())
            }
        }
    }

    /// Parse a macro's Python `source` into the typed [`MacroNode`] tree (the macro body — the
    /// statements inside `def macro(ctx):`). The sidecar's `ast` does the real parsing; this just
    /// frames the request and deserializes the reply. The DUAL of [`crate::macros::nodes_to_source`]
    /// (nodes -> source, in Rust): together they make the visual constructor's mapping two-way.
    ///
    /// A `SyntaxError` in the source is the EXPECTED half-typed case and comes back as
    /// [`ParseError::Syntax`] with the line + message — it never panics or kills the sidecar. No
    /// python runtime / broken pipe / no answer -> [`ParseError::Host`]. Bounded by [`FIRE_BUDGET`];
    /// like [`check`](MacroHost::check), do NOT call from the input/UI thread.
    pub fn parse_macro(&self, source: &str) -> ParseResult {
        let (rx, shared, rid, sent) = {
            let mut g = self.inner.lock().unwrap();
            self.ensure_locked(&mut g).map_err(host_err)?;
            let s = g.session.as_mut().unwrap();
            let rid = s.shared.next_rid.fetch_add(1, Ordering::Relaxed);
            let (tx, rx) = channel();
            let shared = Arc::clone(&s.shared);
            shared.pending.lock().unwrap().insert(rid, tx);
            let ok = s.send(&json!({"t": "parse", "rid": rid, "source": source}));
            (rx, shared, rid, ok)
        };
        if !sent {
            shared.pending.lock().unwrap().remove(&rid);
            return Err(host_err("sidecar pipe broken"));
        }
        match rx.recv_timeout(FIRE_BUDGET) {
            Ok(v) if v.get("ok").and_then(Value::as_bool) == Some(true) => {
                let nodes = v.get("nodes").cloned().unwrap_or(Value::Null);
                serde_json::from_value::<Vec<MacroNode>>(nodes).map_err(|e| {
                    host_err(format!("sidecar returned malformed node JSON: {e}"))
                })
            }
            // a parse that ran but found a SyntaxError -> the structured {line, msg} error.
            Ok(v) => {
                let err = v.get("error");
                let line = err
                    .and_then(|e| e.get("line"))
                    .and_then(Value::as_u64)
                    .unwrap_or(0) as u32;
                let msg = err
                    .and_then(|e| e.get("msg"))
                    .and_then(Value::as_str)
                    .unwrap_or("syntax error")
                    .to_string();
                Err(ParseError::Syntax { line, msg })
            }
            Err(_) => {
                shared.pending.lock().unwrap().remove(&rid);
                Err(host_err("sidecar did not answer"))
            }
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
        // try_lock — if another thread is mid-spawn holding the lock, do NOT wait; fall to "warming".
        if let Ok(mut g) = self.inner.try_lock() {
            let warm = g
                .session
                .as_ref()
                .map(|s| {
                    let st = s.shared.state.lock().unwrap();
                    st.warm && !st.dead
                })
                .unwrap_or(false);
            if warm {
                let s = g.session.as_mut().unwrap();
                let rid = s.shared.next_rid.fetch_add(1, Ordering::Relaxed);
                if s.send(&json!({"t": "fire", "rid": rid, "id": id, "ctx": ctx_json(ctx, armed), "options": load_option_values(id), "mock": mock})) {
                    return format!("macro '{id}' dispatched");
                }
                // pipe broke between the warm-check and the write — the sidecar just died. The fire
                // is genuinely lost (the non-blocking contract forbids re-queueing here); say so.
                g.session = None;
                drop(g);
                self.spawn_background_warm();
                return format!("macro '{id}' dropped (sidecar died — warming, press again)");
            }
            drop(g);
        }
        // not warm (or contended): kick a deduplicated background warm and report — never block.
        self.spawn_background_warm();
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
            let mut g = self.inner.lock().unwrap();
            if let Err(e) = self.ensure_locked(&mut g) {
                return format!("[{e}]");
            }
            let armed = self.armed.load(Ordering::SeqCst);
            let s = g.session.as_mut().unwrap();
            let rid = s.shared.next_rid.fetch_add(1, Ordering::Relaxed);
            let (tx, rx) = channel();
            let shared = Arc::clone(&s.shared);
            shared.pending.lock().unwrap().insert(rid, tx);
            let ok = s.send(&json!({"t": "fire", "rid": rid, "id": id, "ctx": ctx_json(ctx, armed), "options": load_option_values(id)}));
            (rx, shared, rid, ok)
        };
        if !sent {
            shared.pending.lock().unwrap().remove(&rid);
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
                shared.pending.lock().unwrap().remove(&rid);
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
        let mut g = self.inner.lock().unwrap();
        self.ensure_locked(&mut g)
    }

    /// Drain the macro-log ring (the sidecar's stderr: prints + tracebacks). Newest last. Reads the
    /// persistent MacroHost-level ring, so a crashed sidecar's final lines survive its respawn.
    pub fn drain_log(&self) -> Vec<String> {
        self.log.lock().unwrap().drain(..).collect()
    }

    // ── internals ──────────────────────────────────────────────────────────────────────────

    /// Ensure a warm session exists (spawn + register-all if not). Caller holds the inner lock.
    fn ensure_locked(&self, g: &mut Inner) -> Result<(), String> {
        let dead = g
            .session
            .as_ref()
            .map(|s| s.shared.state.lock().unwrap().dead)
            .unwrap_or(true);
        if !dead {
            return Ok(());
        }
        if g.session.is_some() {
            // the old session died — count it and drop it (Drop reaps the process).
            g.breaker.record_crash();
            g.session = None;
        }
        if g.breaker.tripped() {
            return Err(
                "macro sidecar disabled (crashed repeatedly — re-enable in Settings)".into(),
            );
        }
        // Sync the on-disk macros (macros/scripts/*.py) into the manifest at EVERY spawn — a macro
        // saved by `macro add`/the GUI must exist for `macro run <name>` too, not only after the
        // GUI's warm-up happened to run. In-memory sources win (or_insert): a just-registered
        // edit must not be shadowed by a stale file read.
        for (id, src) in scan_macro_dir() {
            g.manifest.entry(id).or_insert(src);
        }
        // a spawn/warm TIMEOUT is also a crash for breaker purposes — otherwise a sidecar that boots
        // but never sends `ready` would be respawned forever (a ~20s storm the breaker exists to stop).
        let session = match spawn_session(
            self.armed.load(Ordering::SeqCst),
            self.log.clone(),
            self.beacon.clone(),
        ) {
            Ok(s) => s,
            Err(e) => {
                g.breaker.record_crash();
                return Err(e);
            }
        };
        // a healthy warm clears the crash history (the breaker only trips on a real storm).
        g.breaker.reset();
        // register every known macro independently (one bad macro must not brick the rest). The
        // reader surfaces any registration failure to the log ring (no waiter needed here).
        let mut sess = session;
        for (id, src) in &g.manifest {
            let rid = sess.shared.next_rid.fetch_add(1, Ordering::Relaxed);
            let _ = sess.send(&json!({"t": "register", "rid": rid, "id": id, "source": src}));
        }
        g.session = Some(sess);
        Ok(())
    }

    /// Kick a background thread to warm the sidecar without blocking the caller (the fire path).
    /// Deduplicated: at most ONE background warm exists at a time, so a burst of cold fires (the
    /// normal state right after a crash) can't spawn a thread each.
    fn spawn_background_warm(&self) {
        let me: &'static MacroHost = macro_host();
        if me
            .warm_in_flight
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return; // a warm is already in flight
        }
        std::thread::spawn(move || {
            let _ = me.ensure_warm();
            me.warm_in_flight.store(false, Ordering::Release);
        });
    }
}

/// Spawn one sidecar process + its reader/logger threads, and wait until the `ready` frame arrives.
/// `log` is the persistent MacroHost-level ring this session feeds (so its output outlives it);
/// `beacon` is the Macro Host-level prompt-listener slot the reader routes ask/notify frames to.
fn spawn_session(armed: bool, log: LogRing, beacon: BeaconSlot) -> Result<Session, String> {
    let rt = resolve_runtime()?;

    let mut cmd = Command::new(&rt.python);
    cmd.arg(&rt.host_script)
        .current_dir(&rt.host_dir) // so `import neuron` finds the co-located host/neuron.py
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(windows)]
    cmd.creation_flags(CREATE_NO_WINDOW);

    let mut child = cmd
        .spawn()
        .map_err(|e| format!("spawn python sidecar: {e}"))?;

    // hand the sidecar's PID to the profiler so it can sample the Python process's CPU.
    crate::prof::SIDECAR_PID.store(child.id(), std::sync::atomic::Ordering::Relaxed);

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
    let reader = {
        let shared = shared.clone();
        let stdin_weak = Arc::downgrade(&stdin);
        std::thread::Builder::new()
            .name("macro-host-reader".into())
            .spawn(move || reader_loop(stdout, shared, beacon, stdin_weak))
            .ok()
    };
    // logger thread: macro output off child STDERR -> bounded ring.
    let logger = {
        let shared = shared.clone();
        std::thread::Builder::new()
            .name("macro-host-logger".into())
            .spawn(move || {
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
        let mut st = shared.state.lock().unwrap();
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
    let mut slot = beacon.lock().unwrap();
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
fn run_act(verb: &str, arg: &Value) -> (bool, String) {
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
    match crate::intent::run_shared_intent(&mut devices, &mut cursor, &intent) {
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
                let mut st = shared.state.lock().unwrap();
                st.warm = true;
                st.dead = false;
                shared.cv.notify_all();
            }
            Some("result") | Some("checked") | Some("pong") | Some("registered")
            | Some("parsed") => {
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
                    .and_then(|rid| shared.pending.lock().unwrap().remove(&rid));
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
                            &mut *stdin.lock().unwrap(),
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
                let stdin = stdin.clone();
                std::thread::spawn(move || {
                    let (ok, msg) = run_act(&verb, &arg);
                    if let Some(stdin) = stdin.upgrade() {
                        let _ = send_frame(
                            &mut *stdin.lock().unwrap(),
                            &json!({"t": "act_result", "rid": rid, "ok": ok, "msg": msg}),
                        );
                    }
                });
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

/// Directory holding python macro sources (cwd-relative, like the rest of Neuron's config).
pub fn macros_dir() -> PathBuf {
    PathBuf::from("macros").join("scripts")
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

fn write_macro_file(id: &str, source: &str) -> Result<(), String> {
    let dir = macros_dir();
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    std::fs::write(macro_path(id), source).map_err(|e| e.to_string())
}

// ── self-describing-option VALUES: chosen in the GUI (or hand-edited), kept beside the scripts ──
fn options_dir() -> PathBuf {
    PathBuf::from("macros").join("options")
}

fn options_path(id: &str) -> PathBuf {
    options_dir().join(format!("{}.json", sanitize_id(id)))
}

/// The user's chosen option values for `id` (a `{key: value}` object). `{}` if none on disk.
fn load_option_values(id: &str) -> Value {
    std::fs::read_to_string(options_path(id))
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .filter(Value::is_object)
        .unwrap_or_else(|| json!({}))
}

fn write_option_values(id: &str, values: &Value) -> Result<(), String> {
    std::fs::create_dir_all(options_dir()).map_err(|e| e.to_string())?;
    let body = serde_json::to_string_pretty(values).map_err(|e| e.to_string())?;
    std::fs::write(options_path(id), body).map_err(|e| e.to_string())
}

/// On (re)register, fill in any option the user hasn't set yet with its declared default — so a
/// freshly-added plugin runs with sensible values and never sees a missing key.
fn seed_option_defaults(id: &str, manifest: &Value) {
    let Some(opts) = manifest.as_array() else {
        return;
    };
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
    let mut out = Vec::new();
    if let Ok(rd) = std::fs::read_dir(macros_dir()) {
        for e in rd.flatten() {
            let p = e.path();
            if p.extension().and_then(|x| x.to_str()) == Some("py") {
                if let (Some(stem), Ok(src)) = (
                    p.file_stem().and_then(|s| s.to_str()),
                    std::fs::read_to_string(&p),
                ) {
                    out.push((stem.to_string(), src));
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
    std::fs::read_to_string(macro_path(id)).ok()
}

/// Free-function convenience over [`MacroHost::parse_macro`] on the process-global host (mirrors how
/// `register`/`scan_macro_dir` are reachable both as methods and module functions). Parses macro
/// `source` into the typed [`MacroNode`] tree; pair with [`crate::macros::nodes_to_source`] for the
/// inverse. Do NOT call from the input/UI thread (it can block up to [`FIRE_BUDGET`]).
pub fn parse_macro(source: &str) -> ParseResult {
    macro_host().parse_macro(source)
}

/// Delete a macro's source file (`macros/scripts/<id>.py`) by id.
///
/// Removing the file is sufficient: the warm sidecar re-syncs its registry from disk on its next
/// spawn, so there's no separate deregister step. The id is sanitised to the same on-disk stem used
/// when the file was written.
pub fn delete_macro(id: &str) -> std::io::Result<()> {
    std::fs::remove_file(macro_path(id))
}

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
}
