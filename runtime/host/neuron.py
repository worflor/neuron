# SPDX-FileCopyrightText: 2026 Woflo Labs
# SPDX-License-Identifier: GPL-3.0-or-later
# Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.
"""
neuron — the host module every Macro Host macro can use.

RAW macros are ordinary full-power CPython. BOUND macros receive this module only through a public
capability proxy: computation stays Python, while machine effects are brokered by the Rust host.
The same helper names work in both modes; arbitrary process execution requires RAW.

SAFE/disarm is enforced twice for brokered effects: the helper gives immediate feedback and Rust
re-checks before acting. RAW code may deliberately bypass helpers through ctypes; that is the explicit
authority escalation selected by "# neuron: raw".
"""

import sys
import os
import time
import threading
import subprocess

__all__ = [
    "ctx", "armed",
    "key", "key_down", "key_up", "type_text", "type_ghost", "hotkey",
    "mouse_move", "mouse_to", "click", "scroll",
    "clipboard_get", "clipboard_set",
    "run", "sleep", "focus",
    "ask", "choose", "confirm", "notify", "log",
    "option", "options",
    "invoke", "store", "load", "forget", "stored",
    "dpi", "dpi_cycle", "scroll_cycle", "profile", "profile_cycle", "brightness",
    "mic_mute", "out_mute", "mic_gain", "out_gain",
    "battery", "current_dpi", "active_profile", "scroll_stage",
]

_IS_WIN = sys.platform == "win32"
_MODE = os.environ.get("NEURON_MACRO_MODE", "raw").strip().lower()
_BOUND = _MODE == "bound"
_armed = False


def _set_armed(on):
    global _armed
    _armed = bool(on)


def armed():
    """Is real input synthesis currently armed? (SAFE mode = False.)"""
    return _armed


def _gated():
    """Should an effectful helper SUPPRESS its action right now? True when input is disarmed (SAFE
    mode) OR this fire is a MOCK fire (a GUI 'test' run — see _set_mock). ask()/notify() never consult
    this: a mock fire raises the macro's REAL beacon, it just performs none of its real-world actions."""
    return (not _armed) or getattr(_tls, "mock", False)


# ── the captured world (read-only snapshot at trigger time) ─────────────────────────────────────
class Ctx:
    __slots__ = ("app", "title", "cwd", "clipboard", "selection", "prev_window", "armed")

    def __init__(self, d=None):
        d = d or {}
        self.app = d.get("app")
        self.title = d.get("title")
        self.cwd = d.get("cwd")
        self.clipboard = d.get("clipboard")
        self.selection = d.get("selection")
        self.prev_window = d.get("prev_window", 0)
        self.armed = d.get("armed", False)

    def __repr__(self):
        return "Ctx(app=%r, title=%r)" % (self.app, self.title)


# Fires run CONCURRENTLY (one serial worker per macro id), so the "current" ctx is per-thread —
# a slow macro awaiting a beacon answer must not see a later fire's world. `neuron.ctx` resolves
# through the module-level __getattr__ below, so existing macros keep reading it unchanged.
_tls = threading.local()


def _set_ctx(d):
    # The per-fire ctx carries an `armed` flag for the macro to READ (ctx.armed), but it must NOT
    # move the gate: the single source of truth for the input layer is the `armed` control frame
    # routed through _set_armed(). Letting a fire's ctx flip _armed would race the control channel.
    _tls.ctx = Ctx(d)


def _set_mid(mid):
    """Record which macro id this worker thread is firing (names beacons/notifies honestly)."""
    _tls.mid = mid


def _set_options(d):
    """The user's chosen values for THIS macro's declared options (per-fire, per-thread)."""
    _tls.options = dict(d or {})


def _set_mock(on):
    """Mark THIS worker's current fire as a MOCK/test run (per-thread, set fresh on every fire). While
    set, every effectful helper no-ops exactly as SAFE mode does — but ask()/notify() still reach the
    human, so the GUI 'test' button raises a macro's REAL beacon without performing its real actions."""
    _tls.mock = bool(on)


def _get_ctx():
    return getattr(_tls, "ctx", None) or Ctx()


# ── self-describing options: a macro declares `NEURON_OPTIONS`, the GUI renders controls, and
# the chosen values arrive here at fire time. Read them with option('key', default) / options(). ──
def option(key, default=None):
    """The user's chosen value for the declared option `key` (or `default` if unset/undeclared).

    Declare options at module scope so Neuron can surface real controls for them:

        NEURON_OPTIONS = [
            {"key": "speed", "label": "Typing speed", "type": "choice",
             "choices": ["instant", "borderline", "fast", "normal"], "default": "fast"},
            {"key": "phrases", "label": "Phrases (one per line)", "type": "text", "default": ""},
        ]

    Types: "string" | "text" (multiline) | "choice" (needs "choices") | "number" | "bool".
    """
    return getattr(_tls, "options", {}).get(key, default)


def options():
    """The whole option-values dict for this fire (a copy)."""
    return dict(getattr(_tls, "options", {}))


def __getattr__(name):
    # module-level dynamic attribute: `neuron.ctx` is the CURRENT THREAD's fire context.
    if name == "ctx":
        return _get_ctx()
    raise AttributeError("module 'neuron' has no attribute %r" % name)


# ── the beacon layer (two-part macros: prime, then activate through the radial) ─────────────────
# The host process injects `_host_send` (the framed protocol writer) after import. ask()/notify()
# are NOT arm-gated: they synthesize no input — they only ask the human. SAFE mode keeps them.
_host_send = None
_ask_lock = threading.Lock()
_ask_seq = [0]
# Keep prompt ids globally distinct even though RAW and BOUND allocate them independently.
_ASK_PREFIX = (1 << 63) if _BOUND else 0
_asks = {}  # pid -> {"event": Event, "choice": int|None}  (chosen option index, or None = passed)


def _set_host_send(fn):
    global _host_send
    _host_send = fn


def _deliver_answer(pid, choice):
    """Host -> here: the user's pick (an option index, or null = passed). Expired pids are ignored."""
    with _ask_lock:
        slot = _asks.get(pid)
        if slot is None:
            return
        slot["choice"] = choice if isinstance(choice, int) and choice >= 0 else None
        slot["event"].set()


def _prompt(text, options, timeout=300, description=""):
    """THE prompt core — ask/choose/confirm are all thin wrappers over this ONE path. Present `options`
    as the answer wheel, BLOCK (this macro only) until the user flicks one, and return the chosen
    INDEX — or None if passed / timed out / no UI. The N=2 wheel IS yes/no/pass; an N-wide wheel is a
    radial menu; N=1 is a single confirm. Other macros keep firing while this one waits; never raises."""
    opts = [str(o) for o in options]
    if _host_send is None or not opts:
        return None
    ev = threading.Event()
    with _ask_lock:
        _ask_seq[0] += 1
        pid = _ASK_PREFIX | _ask_seq[0]
        _asks[pid] = {"event": ev, "choice": None}
    _host_send({
        "t": "prompt", "pid": pid, "id": getattr(_tls, "mid", "?"),
        "text": str(text), "options": opts, "detail": str(description), "timeout": timeout,
    })
    answered = ev.wait(timeout)
    with _ask_lock:
        slot = _asks.pop(pid, None)
    if not answered:
        # timed out sidecar-side — tell the host so any UI still showing the prompt retires it.
        try:
            _host_send({"t": "prompt_done", "pid": pid, "why": "timeout"})
        except Exception:
            pass
        return None
    return slot["choice"] if slot else None


def ask(text, default=None, timeout=300, description=""):
    """BEACON: ask a yes/no question and BLOCK (this macro only) until the user answers — the 2-option
    case of the one prompt wheel. The user holds the cast trigger and flicks toward your ACCENT
    (west/left) for yes, the raw MATERIAL (east/right) for no, or passes (flick away / release) to get
    `default`. Returns True / False / `default`; never raises. Other macros keep firing while it waits.

    `description` (optional) is context shown UNDER the wheel — e.g.
    `neuron.ask("overwrite save?", description="3 uncommitted files will be lost")`."""
    c = _prompt(text, ["yes", "no"], timeout, description)
    if c == 0:
        return True
    if c == 1:
        return False
    return default


def choose(text, options, default=None, timeout=300, description=""):
    """BEACON: present a RADIAL MENU of `options` and BLOCK until the user flicks one — the N-option
    case of the same wheel ask() uses. Returns the chosen option (the string), or `default` if passed /
    timed out / no UI. Keep `options` to what's reliably hittable (the wheel caps wedges by feel).
    Never raises; other macros keep firing while it waits."""
    opts = [str(o) for o in options]
    c = _prompt(text, opts, timeout, description)
    return opts[c] if (c is not None and 0 <= c < len(opts)) else default


def confirm(text, default=False, timeout=300, description=""):
    """BEACON: a review BREAKPOINT — pause the macro and glance. The whole wheel is live, so ANY flick
    commits (no aiming); leaving it alone (or letting it time out) takes the default and the macro just
    proceeds. Returns True only if you actively SWIPED, else `default`. So it's a human-in-the-loop
    pause: `if neuron.confirm("about to wipe the folder"): return` gives you a window to intervene,
    while doing nothing runs the default path. Simpler than a yes/no by design. Never raises."""
    return True if _prompt(text, [""], timeout, description) == 0 else default


def notify(text):
    """BEACON (fire-and-forget): surface a one-line status to Neuron's readout + macro log.
    Use it to say "primed", "deploy started", "3 results ready" — anything worth a glance.
    Not arm-gated (no input synthesis). Returns True if the host heard it."""
    if _host_send is None:
        return False
    _host_send({"t": "notify", "id": getattr(_tls, "mid", "?"), "text": str(text)})
    return True


def log(*args):
    """Write a line to Neuron's macro log (the readout's log tail). Like print(), but explicitly to
    the log channel — use notify() for a glanceable beacon, log() for quieter diagnostics. Never
    raises; not arm-gated (it writes no input)."""
    try:
        sys.stderr.write(" ".join(str(a) for a in args) + "\n")
    except Exception:
        pass
    return True


# ── cross-macro composition: a macro can call ANOTHER registered macro as a subroutine ──────────
# The sidecar holds every macro as a LIVE function (its registry), so invoke() can run a sibling RIGHT
# HERE on this worker thread — synchronous, returning its value — while preserving THIS fire's
# per-thread world (mid / options; the captured ctx is shared down the chain). A depth guard stops a
# runaway cycle (a -> b -> a -> …). The host injects the two hooks below after the registry exists.
_host_lookup = None    # (mid) -> the registered macro fn, or None
_host_dispatch = None  # (fire_msg) -> enqueue a fire on a macro's own serial worker
_INVOKE_MAX_DEPTH = 16


def _set_host_lookup(fn):
    global _host_lookup
    _host_lookup = fn


def _set_host_dispatch(fn):
    global _host_dispatch
    _host_dispatch = fn


def _ctx_payload():
    """The current fire's captured world as a plain dict (to hand to an async-invoked fire)."""
    c = _get_ctx()
    return {"app": c.app, "title": c.title, "cwd": c.cwd, "clipboard": c.clipboard,
            "selection": c.selection, "prev_window": c.prev_window, "armed": c.armed}


def invoke(name, wait=True, **opts):
    """Run another macro by id.

    Same-domain calls keep their existing direct semantics. Cross-domain calls go back through the
    Rust host so a RAW caller may invoke BOUND without moving that callee into the RAW interpreter.
    Authority is monotonic: BOUND -> RAW is refused. Cross-domain return values cross the protocol
    as strings; same-domain synchronous calls still return the original Python value.
    """
    fn = _host_lookup(name) if _host_lookup else None
    if fn is None:
        raw = _act(
            "invoke",
            {
                "id": str(name),
                "wait": bool(wait),
                "ctx": _ctx_payload(),
                "options": dict(opts),
            },
            timeout=305.0 if wait else 5.0,
            gated=False,
        )
        try:
            payload = _json.loads(raw)
        except Exception:
            return None
        if not payload.get("found"):
            return None
        if payload.get("error"):
            sys.stderr.write("[neuron.invoke %s] %s\n" % (name, payload.get("error")))
            return None
        return payload.get("value") if wait else None

    if not wait:
        if _host_dispatch:
            _host_dispatch({
                "id": name,
                "rid": None,
                "ctx": _ctx_payload(),
                "options": dict(opts),
                "mock": getattr(_tls, "mock", False),
            })
        return None

    depth = getattr(_tls, "invoke_depth", 0)
    if depth >= _INVOKE_MAX_DEPTH:
        return None
    saved_mid = getattr(_tls, "mid", None)
    saved_opts = getattr(_tls, "options", None)
    try:
        _tls.invoke_depth = depth + 1
        _tls.mid = name
        _tls.options = dict(opts)
        return fn(_get_ctx())
    except BaseException:
        import traceback as _tb
        sys.stderr.write("[neuron.invoke %s]\n%s" % (name, _tb.format_exc()))
        return None
    finally:
        _tls.invoke_depth = depth
        _tls.mid = saved_mid
        _tls.options = saved_opts


# ── persistent state: a tiny per-macro key/value store that survives across fires AND restarts ──
# JSON on disk, one file per macro id (a stable per-user dir; override with NEURON_MACRO_STATE).
# Writes are atomic (temp + os.replace) so a crash mid-write can't corrupt the store. Each macro
# owns its own lock: load→mutate→replace stays serialized within ONE namespace while unrelated
# macros can persist concurrently instead of head-of-line blocking behind somebody else's AV retry.
#
# UNDER LOAD: Windows Defender / Search Indexer / backup agents routinely grab a *transient* handle
# on a file the instant it's created or renamed — open()/os.replace() then raises PermissionError
# ("Access is denied", WinError 5) or OSError 32 (sharing violation) for a few milliseconds, even
# though nothing in-process is contending (the per-macro lock already rules that out). This has
# nothing to do with our own concurrency: it's an external, self-clearing lock on the file. The fix
# is a short bounded retry around the actual filesystem calls — NOT a broader lock, since the
# contender isn't another thread of ours. A real (non-transient) I/O error still surfaces after the
# retry budget is exhausted, so genuine failures aren't hidden.
import json as _json  # noqa: E402
import time as _time  # noqa: E402

_state_locks = {}
_state_locks_guard = threading.Lock()


def _state_lock_for(mid):
    key = str(mid)
    with _state_locks_guard:
        lock = _state_locks.get(key)
        if lock is None:
            lock = threading.Lock()
            _state_locks[key] = lock
        return lock


# Windows AV/indexer handle-steals clear in low single-digit milliseconds; 40 tries * 25ms = up to 1s
# of retrying before we give up, which is generous for a "cold, never a hot path" store.
_STATE_IO_RETRIES = 40
_STATE_IO_RETRY_DELAY = 0.025


def _retry_transient(fn):
    """Run fn() (an actual filesystem op), retrying ONLY the transient sharing/permission errors an
    external process (AV/indexer/backup) can momentarily impose on a file we just created or renamed.
    Re-raises the last error once the retry budget is spent, so a genuine failure still surfaces."""
    last = None
    for attempt in range(_STATE_IO_RETRIES):
        try:
            return fn()
        except (PermissionError, OSError) as e:
            # WinError 5 = access denied, WinError 32 = sharing violation — both are the external
            # transient-handle pattern. Anything else (disk full, path too long, ...) isn't transient;
            # fail fast instead of burning the retry budget on something that'll never clear.
            winerr = getattr(e, "winerror", None)
            if winerr not in (5, 32) and not isinstance(e, PermissionError):
                raise
            last = e
            if attempt == _STATE_IO_RETRIES - 1:
                raise
            _time.sleep(_STATE_IO_RETRY_DELAY)
    raise last  # pragma: no cover — loop always returns or raises above


def _state_dir():
    base = os.environ.get("NEURON_MACRO_STATE")
    if not base:
        root = (os.environ.get("LOCALAPPDATA") or os.environ.get("XDG_DATA_HOME")
                or os.path.join(os.path.expanduser("~"), ".local", "share"))
        base = os.path.join(root, "neuron", "macro_state")
    return base


def _state_path(mid):
    safe = "".join(c if (c.isalnum() or c in "-_.") else "_" for c in str(mid)) or "_"
    return os.path.join(_state_dir(), safe + ".json")


def _state_read(mid):
    def _do():
        with open(_state_path(mid), "r", encoding="utf-8") as f:
            d = _json.load(f)
            return d if isinstance(d, dict) else {}
    try:
        return _retry_transient(_do)
    except FileNotFoundError:
        return {}  # no store yet — a legitimate, non-transient "empty" state
    except Exception:
        return {}  # corrupt JSON / anything else non-transient — degrade gracefully, never raise


def _state_write(mid, d):
    p = _state_path(mid)

    def _do():
        os.makedirs(os.path.dirname(p), exist_ok=True)
        tmp = p + ".tmp"
        with open(tmp, "w", encoding="utf-8") as f:
            _json.dump(d, f, ensure_ascii=False)
        os.replace(tmp, p)  # atomic — a crash mid-write leaves the prior store intact

    try:
        _retry_transient(_do)
        return True
    except Exception:
        return False


def store(key, value):
    """Persist a JSON-able value under key for THIS macro."""
    if _BOUND:
        try:
            _json.dumps(value)
        except Exception:
            return False
        return _act("state_store", {"key": str(key), "value": value}, gated=False) == "true"
    mid = getattr(_tls, "mid", "?")
    with _state_lock_for(mid):
        d = _state_read(mid)
        d[str(key)] = value
        return _state_write(mid, d)


def load(key, default=None):
    """Read a value saved by store() for THIS macro (or default if unset). Never raises."""
    if _BOUND:
        raw = _act("state_load", {"key": str(key)}, gated=False)
        try:
            payload = _json.loads(raw)
            return payload.get("value") if payload.get("found") else default
        except Exception:
            return default
    mid = getattr(_tls, "mid", "?")
    with _state_lock_for(mid):
        return _state_read(mid).get(str(key), default)


def forget(key=None):
    """Delete one stored key, or (key=None) wipe THIS macro's whole store."""
    if _BOUND:
        arg = None if key is None else str(key)
        return _act("state_forget", arg, gated=False) == "true"
    mid = getattr(_tls, "mid", "?")
    with _state_lock_for(mid):
        if key is None:
            try:
                os.remove(_state_path(mid))
            except FileNotFoundError:
                pass
            except Exception:
                return False
            return True
        d = _state_read(mid)
        d.pop(str(key), None)
        return _state_write(mid, d)


def stored():
    """The whole stored dict for THIS macro (a copy)."""
    if _BOUND:
        raw = _act("state_stored", gated=False)
        try:
            value = _json.loads(raw)
            return value if isinstance(value, dict) else {}
        except Exception:
            return {}
    mid = getattr(_tls, "mid", "?")
    with _state_lock_for(mid):
        return dict(_state_read(mid))


# ── device / app control: a macro can drive NEURON ITSELF (DPI, profile, scroll stage) ──────────
# These cross to the host, which runs the SAME shared intent a bound trigger uses — so a macro's DPI
# change is a real, confirmed device write that even raises the same confirmation card. Each helper
# BLOCKS this worker briefly for the outcome (a device round-trip is milliseconds) with a short safety
# timeout, and is arm-gated (a real-world side effect — SAFE mode / a test fire suppresses it).
_act_lock = threading.Lock()
_act_seq = [0]
_acts = {}  # rid -> {"event": Event, "ok": bool, "msg": str|None}


def _deliver_act(rid, ok, msg):
    """Host -> here: a device verb finished. Wake the waiting helper. Unknown/expired rids ignored."""
    with _act_lock:
        slot = _acts.get(rid)
        if slot is None:
            return
        slot["ok"] = bool(ok)
        slot["msg"] = msg
        slot["event"].set()


def _act(verb, arg=None, timeout=5.0, gated=True):
    """Send a device verb to the host and block (briefly) for its result string. Effectful verbs are
    arm-gated (gated=True); read-back verbs pass gated=False — reading state has no side effect."""
    if _host_send is None:
        return "[no host]"
    if gated and _gated():
        return "[disarmed]"
    ev = threading.Event()
    with _act_lock:
        _act_seq[0] += 1
        rid = _act_seq[0]
        _acts[rid] = {"event": ev, "ok": False, "msg": None}
    _host_send({
        "t": "act", "rid": rid, "id": getattr(_tls, "mid", "?"),
        "verb": verb, "arg": arg, "mock": getattr(_tls, "mock", False),
    })
    done = ev.wait(timeout)
    with _act_lock:
        slot = _acts.pop(rid, None)
    if not done:
        return "[timed out]"
    if slot and slot["msg"] is not None:
        return slot["msg"]
    return "ok" if slot and slot["ok"] else "[failed]"


def dpi(value):
    """Set the mouse DPI to an absolute value (clamped to the device's range). Arm-gated. Returns the
    host's confirmation string (e.g. 'DPI -> 1600')."""
    return _act("dpi", int(value))


def dpi_cycle(direction="up"):
    """Cycle DPI to the next/previous stage of the active profile. direction='up'|'down'. Arm-gated."""
    return _act("dpi_cycle", str(direction))


def scroll_cycle(direction="up"):
    """Cycle the HyperScroll wheel stage. direction='up'|'down'. Arm-gated."""
    return _act("scroll_cycle", str(direction))


def profile(name):
    """Switch to a saved Neuron profile by name. Arm-gated. Returns the host's confirmation string."""
    return _act("profile", str(name))


def profile_cycle(direction="up"):
    """Cycle to the next/previous saved profile. direction='up'|'down'. Arm-gated."""
    return _act("profile_cycle", str(direction))


def brightness(pct):
    """Set the device lighting brightness to `pct` (0-100). Arm-gated. Returns the host's message."""
    return _act("brightness", int(pct))


def mic_mute(mode="toggle"):
    """Mute/unmute the system microphone. mode='on'|'off'|'toggle'. Arm-gated."""
    return _act("mic_mute", str(mode))


def out_mute(mode="toggle"):
    """Mute/unmute the system output (speakers). mode='on'|'off'|'toggle'. Arm-gated."""
    return _act("out_mute", str(mode))


def mic_gain(delta_pct):
    """Nudge the microphone volume by `delta_pct` (e.g. +5 / -10). Arm-gated."""
    return _act("mic_gain", float(delta_pct))


def out_gain(delta_pct):
    """Nudge the output volume by `delta_pct` (e.g. +5 / -10). Arm-gated."""
    return _act("out_gain", float(delta_pct))


# ── SENSE: read live device / app state so a macro can react to it (reads are never arm-gated) ──
def battery():
    """The mouse battery as {"percent": int, "charging": bool}, or None if unreadable."""
    s = _act("battery", gated=False)
    try:
        p, c = s.split("|")
        return {"percent": int(p), "charging": c.strip().lower() == "true"}
    except Exception:
        return None


def current_dpi():
    """The mouse's current DPI as an int, or None if unreadable."""
    s = _act("current_dpi", gated=False)
    try:
        return int(s)
    except Exception:
        return None


def active_profile():
    """The name of the currently-active Neuron profile (a string; '' if none/unknown)."""
    s = _act("active_profile", gated=False)
    return s if (s and not s.startswith("[")) else ""


def scroll_stage():
    """The current HyperScroll wheel stage as an int (1-based), or None if unknown."""
    s = _act("scroll_stage", gated=False)
    try:
        return int(s)
    except Exception:
        return None


# ── OBS: drive & sense OBS Studio (needs SYSTEM → CONNECTIONS with obs on) ──────────────────────
def obs_scene(name):
    """Switch OBS to the named scene. Works with OBS minimized (a real obs-websocket request,
    no phantom hotkeys). Arm-gated. Returns the host's confirmation string."""
    return _act("obs_scene", str(name))


def obs_stream(mode="toggle"):
    """Start/stop/toggle the OBS stream. mode='start'|'stop'|'toggle'. Arm-gated."""
    return _act("obs_stream", str(mode))


def obs_record(mode="toggle"):
    """Start/stop/toggle OBS recording; mode='pause' toggles pause/resume on a running one.
    mode='start'|'stop'|'toggle'|'pause'. Arm-gated."""
    return _act("obs_record", str(mode))


def obs_replay(mode="save"):
    """The OBS replay buffer — the 'clip that!' button. mode='save' (default) writes the clip;
    'start'/'stop' run the buffer itself (it must be running before a save can land). Arm-gated."""
    return _act("obs_replay", str(mode))


def obs_mute(input_name="Mic/Aux"):
    """Toggle mute on a named OBS input (OBS's own mute, distinct from the system mic). Arm-gated."""
    return _act("obs_mute", str(input_name))


def obs_request(request_type, data=None):
    """Fire ANY obs-websocket v5 request — the whole OBS API with no per-request code, e.g.
    obs_request("SaveReplayBuffer") or
    obs_request("SetInputVolume", {"inputName": "Mic/Aux", "inputVolumeDb": -6}).
    Fire-and-forget: the response body does not come back. Arm-gated."""
    if data is None:
        return _act("obs_request", str(request_type))
    return _act("obs_request", {"type": str(request_type), "data": data})


def signal(channel, value=1.0):
    """Drive a Signal lighting channel (1-4): the 'Signal' layer on the LIGHTING page renders the
    channel wherever the user painted it. value 0..1 (0 = dark; on the default spectrum the value
    reads as urgency green->red). The value STAYS until overwritten — a CI light keeps burning red
    until a macro turns it green. Not arm-gated: it's engine state, like store()."""
    return _act("signal", {"ch": int(channel), "value": float(value)}, gated=False)


def obs_connected():
    """Is neuron's websocket to OBS authenticated right now? (bool)"""
    return _act("obs_get", "connected", gated=False) == "true"


def obs_scene_name():
    """The current OBS program scene name ('' if unknown or not connected)."""
    s = _act("obs_get", "scene", gated=False)
    return "" if s.startswith("[") or s.startswith("OBS not connected") else s


def obs_streaming():
    """Is the stream live? (bool; False when unknown or not connected)"""
    return _act("obs_get", "streaming", gated=False) == "true"


def obs_recording():
    """Is OBS recording? (bool; False when unknown or not connected)"""
    return _act("obs_get", "recording", gated=False) == "true"


# ── Windows input layer (ctypes resolves the union/ABI itself — robust by construction) ─────────
if _IS_WIN:
    import ctypes
    from ctypes import wintypes

    _user32 = ctypes.WinDLL("user32", use_last_error=True)
    _kernel32 = ctypes.WinDLL("kernel32", use_last_error=True)
    _ULONG_PTR = ctypes.c_size_t

    # CRITICAL: declare pointer-returning/taking prototypes. ctypes defaults a return type to a
    # 32-bit c_int, which TRUNCATES a 64-bit HANDLE/HWND on x64 -> a bad pointer -> access
    # violation that kills the whole interpreter. Every handle here is pointer-sized.
    _HWND = ctypes.c_void_p
    _HANDLE = ctypes.c_void_p
    _u = _user32
    _k = _kernel32
    _u.SetForegroundWindow.restype = wintypes.BOOL
    _u.SetForegroundWindow.argtypes = [_HWND]
    _u.FindWindowW.restype = _HWND
    _u.FindWindowW.argtypes = [wintypes.LPCWSTR, wintypes.LPCWSTR]
    _u.GetSystemMetrics.restype = ctypes.c_int
    _u.GetSystemMetrics.argtypes = [ctypes.c_int]
    _u.OpenClipboard.restype = wintypes.BOOL
    _u.OpenClipboard.argtypes = [_HWND]
    _u.CloseClipboard.restype = wintypes.BOOL
    _u.CloseClipboard.argtypes = []
    _u.EmptyClipboard.restype = wintypes.BOOL
    _u.EmptyClipboard.argtypes = []
    _u.GetClipboardData.restype = _HANDLE
    _u.GetClipboardData.argtypes = [wintypes.UINT]
    _u.SetClipboardData.restype = _HANDLE
    _u.SetClipboardData.argtypes = [wintypes.UINT, _HANDLE]
    _u.SendInput.restype = wintypes.UINT
    _u.SendInput.argtypes = [wintypes.UINT, ctypes.c_void_p, ctypes.c_int]
    _k.GlobalAlloc.restype = _HANDLE
    _k.GlobalAlloc.argtypes = [wintypes.UINT, ctypes.c_size_t]
    _k.GlobalLock.restype = ctypes.c_void_p
    _k.GlobalLock.argtypes = [_HANDLE]
    _k.GlobalUnlock.restype = wintypes.BOOL
    _k.GlobalUnlock.argtypes = [_HANDLE]

    class _KEYBDINPUT(ctypes.Structure):
        _fields_ = [("wVk", wintypes.WORD), ("wScan", wintypes.WORD),
                    ("dwFlags", wintypes.DWORD), ("time", wintypes.DWORD),
                    ("dwExtraInfo", _ULONG_PTR)]

    class _MOUSEINPUT(ctypes.Structure):
        _fields_ = [("dx", wintypes.LONG), ("dy", wintypes.LONG),
                    ("mouseData", wintypes.DWORD), ("dwFlags", wintypes.DWORD),
                    ("time", wintypes.DWORD), ("dwExtraInfo", _ULONG_PTR)]

    class _INPUTUNION(ctypes.Union):
        _fields_ = [("ki", _KEYBDINPUT), ("mi", _MOUSEINPUT)]

    class _INPUT(ctypes.Structure):
        _fields_ = [("type", wintypes.DWORD), ("u", _INPUTUNION)]

    _INPUT_MOUSE = 0
    _INPUT_KEYBOARD = 1
    _KEYEVENTF_KEYUP = 0x0002
    _KEYEVENTF_UNICODE = 0x0004
    _MOUSEEVENTF_MOVE = 0x0001
    _MOUSEEVENTF_ABSOLUTE = 0x8000
    _MOUSEEVENTF_LEFTDOWN = 0x0002
    _MOUSEEVENTF_LEFTUP = 0x0004
    _MOUSEEVENTF_RIGHTDOWN = 0x0008
    _MOUSEEVENTF_RIGHTUP = 0x0010
    _MOUSEEVENTF_MIDDLEDOWN = 0x0020
    _MOUSEEVENTF_MIDDLEUP = 0x0040
    _MOUSEEVENTF_WHEEL = 0x0800
    _SM_CXSCREEN = 0
    _SM_CYSCREEN = 1
    _CF_UNICODETEXT = 13
    _GMEM_MOVEABLE = 0x0002

    def _vk(name):
        n = name.strip().lower()
        if len(n) == 1:
            c = n[0]
            if c.isalpha():
                return ord(c.upper())
            if c.isdigit():
                return ord(c)
        if n.startswith("f") and n[1:].isdigit():
            num = int(n[1:])
            if 1 <= num <= 24:
                return 0x70 + (num - 1)
        return {
            "enter": 0x0D, "return": 0x0D, "space": 0x20, "tab": 0x09,
            "esc": 0x1B, "escape": 0x1B, "backspace": 0x08, "delete": 0x2E, "del": 0x2E,
            "shift": 0x10, "ctrl": 0x11, "control": 0x11, "alt": 0x12, "win": 0x5B,
            "up": 0x26, "down": 0x28, "left": 0x25, "right": 0x27,
            "home": 0x24, "end": 0x23, "pageup": 0x21, "pagedown": 0x22,
        }.get(n)

    def _send_keys(inputs):
        n = len(inputs)
        arr = (_INPUT * n)(*inputs)
        _user32.SendInput(n, arr, ctypes.sizeof(_INPUT))

    def _mk_key(vk, up):
        i = _INPUT()
        i.type = _INPUT_KEYBOARD
        i.u.ki = _KEYBDINPUT(vk, 0, _KEYEVENTF_KEYUP if up else 0, 0, 0)
        return i

    def _mk_unicode(ch, up):
        i = _INPUT()
        i.type = _INPUT_KEYBOARD
        flags = _KEYEVENTF_UNICODE | (_KEYEVENTF_KEYUP if up else 0)
        i.u.ki = _KEYBDINPUT(0, ch, flags, 0, 0)
        return i

    def _mk_mouse(flags, dx=0, dy=0, data=0):
        i = _INPUT()
        i.type = _INPUT_MOUSE
        i.u.mi = _MOUSEINPUT(dx, dy, data, flags, 0, 0)
        return i


def key(name):
    """Press a key by name ('f', 'enter', 'f5', 'ctrl'…). down+up. Arm-gated."""
    if _BOUND:
        return _act("key", str(name))
    if not _IS_WIN:
        return "[unsupported]"
    vk = _vk(name)
    if vk is None:
        return "[unknown key %r]" % name
    if _gated():
        return "[disarmed]"
    _send_keys([_mk_key(vk, False), _mk_key(vk, True)])
    return True


def key_down(name):
    if _BOUND:
        return _act("key_down", str(name))
    if not _IS_WIN:
        return "[unsupported]"
    vk = _vk(name)
    if vk is None:
        return "[unknown key %r]" % name
    if _gated():
        return "[disarmed]"
    _send_keys([_mk_key(vk, False)])
    return True


def key_up(name):
    if _BOUND:
        return _act("key_up", str(name))
    if not _IS_WIN:
        return "[unsupported]"
    vk = _vk(name)
    if vk is None:
        return "[unknown key %r]" % name
    if _gated():
        return "[disarmed]"
    _send_keys([_mk_key(vk, True)])
    return True


def hotkey(*keys):
    """Fire a chord: hotkey('ctrl','shift','v'). down in order, up in reverse. Arm-gated."""
    if _BOUND:
        return _act("hotkey", [str(k) for k in keys])
    if not _IS_WIN:
        return "[unsupported]"
    vks = [_vk(k) for k in keys]
    if any(v is None for v in vks):
        return "[unknown key in chord]"
    if _gated():
        return "[disarmed]"
    _send_keys([_mk_key(v, False) for v in vks] + [_mk_key(v, True) for v in reversed(vks)])
    return True


def type_text(s):
    """Type a literal Unicode string as keystrokes, all at once. Arm-gated."""
    if _BOUND:
        return _act("type_text", str(s))
    if not _IS_WIN:
        return "[unsupported]"
    if _gated():
        return "[disarmed]"
    inputs = []
    for ch in s:
        code = ord(ch)
        inputs.append(_mk_unicode(code, False))
        inputs.append(_mk_unicode(code, True))
    if inputs:
        _send_keys(inputs)
    return True


# the SAME base timing the native ghost-paste uses: (base ms/char, jitter ms — the scale of a
# one-sided slow tail, not a symmetric ±). Instant is opt-in.
_GHOST_SPEED = {
    "instant": (0, 0),
    "borderline": (9, 5),
    "fast": (55, 25),
    "normal": (165, 60),
}


def type_ghost(s, speed="borderline"):
    """GHOST-TYPE a string with the native ghost-paste cadence. Arm-gated."""
    if _BOUND:
        # normal pace is ~165ms/char plus pauses; size the reply wait to the requested work.
        timeout = max(5.0, len(str(s)) * 0.35 + 2.0)
        return _act("type_ghost", {"text": str(s), "speed": str(speed)}, timeout=timeout)
    if not _IS_WIN:
        return "[unsupported]"
    if _gated():
        return "[disarmed]"
    import random
    base, jit = _GHOST_SPEED.get(str(speed).lower(), _GHOST_SPEED["borderline"])
    i = 0
    chars = list(s)
    n = len(chars)
    while i < n:
        if _gated():
            return "[disarmed mid-type]"
        ch = chars[i]
        if ch == "\r":
            _send_keys([_mk_key(0x0D, False), _mk_key(0x0D, True)])
            if i + 1 < n and chars[i + 1] == "\n":
                i += 1
        elif ch == "\n":
            _send_keys([_mk_key(0x0D, False), _mk_key(0x0D, True)])
        elif ch == "\t":
            _send_keys([_mk_key(0x09, False), _mk_key(0x09, True)])
        else:
            code = ord(ch)
            _send_keys([_mk_unicode(code, False), _mk_unicode(code, True)])
        i += 1
        # organic keystroke timing: a Gaussian centered on base with a heavier, one-sided SLOW tail
        # (a stalled sort-of-miss, never a negative delay) — the keystroke-dynamics rhythm instead of
        # a flat uniform wobble.
        d = base
        if jit:
            d = max(0.0, base + random.gauss(jit * 0.1, jit * 0.35))
        # a longer beat after word and sentence breaks, like a typist finishing a phrase
        if 0 < i < n:
            prev_c = chars[i - 1]
            if prev_c in ".,;:!?\u2026":
                d *= 2.4
            elif prev_c == " ":
                d *= 1.6
        if d > 0:
            time.sleep(d / 1000.0)
    return True


def mouse_move(dx, dy):
    """Move the cursor by a relative delta. Arm-gated."""
    if _BOUND:
        return _act("mouse_move", {"dx": int(dx), "dy": int(dy)})
    if not _IS_WIN:
        return "[unsupported]"
    if _gated():
        return "[disarmed]"
    _send_keys([_mk_mouse(_MOUSEEVENTF_MOVE, int(dx), int(dy))])
    return True


def mouse_to(x, y):
    """Move the cursor to an absolute primary-screen pixel. Arm-gated."""
    if _BOUND:
        return _act("mouse_to", {"x": int(x), "y": int(y)})
    if not _IS_WIN:
        return "[unsupported]"
    if _gated():
        return "[disarmed]"
    w = max(1, _user32.GetSystemMetrics(_SM_CXSCREEN))
    h = max(1, _user32.GetSystemMetrics(_SM_CYSCREEN))
    ax = int(int(x) * 65535 / w)
    ay = int(int(y) * 65535 / h)
    _send_keys([_mk_mouse(_MOUSEEVENTF_MOVE | _MOUSEEVENTF_ABSOLUTE, ax, ay)])
    return True


def click(button="left"):
    """Click a mouse button ('left'|'right'|'middle'). Arm-gated."""
    if _BOUND:
        return _act("click", str(button))
    if not _IS_WIN:
        return "[unsupported]"
    if _gated():
        return "[disarmed]"
    b = button.strip().lower()
    if b == "right":
        down, up = _MOUSEEVENTF_RIGHTDOWN, _MOUSEEVENTF_RIGHTUP
    elif b == "middle":
        down, up = _MOUSEEVENTF_MIDDLEDOWN, _MOUSEEVENTF_MIDDLEUP
    else:
        down, up = _MOUSEEVENTF_LEFTDOWN, _MOUSEEVENTF_LEFTUP
    _send_keys([_mk_mouse(down), _mk_mouse(up)])
    return True


def scroll(notches):
    """Scroll the wheel by `notches` (positive = up/away). Arm-gated."""
    if _BOUND:
        return _act("scroll", int(notches))
    if not _IS_WIN:
        return "[unsupported]"
    if _gated():
        return "[disarmed]"
    _send_keys([_mk_mouse(_MOUSEEVENTF_WHEEL, 0, 0, int(notches) * 120 & 0xFFFFFFFF)])
    return True


def clipboard_get():
    """Read the clipboard's Unicode text (read-only, never gated). None if empty/non-text."""
    if _BOUND:
        s = _act("clipboard_get", gated=False)
        return None if not s or str(s).startswith("[") else s
    if not _IS_WIN:
        return None
    if _user32.OpenClipboard(0) == 0:
        return None
    try:
        h = _user32.GetClipboardData(_CF_UNICODETEXT)
        if not h:
            return None
        p = _kernel32.GlobalLock(h)
        if not p:
            return None
        try:
            return ctypes.c_wchar_p(p).value
        finally:
            _kernel32.GlobalUnlock(h)
    finally:
        _user32.CloseClipboard()


def clipboard_set(s):
    """Put a string on the clipboard (CF_UNICODETEXT). Arm-gated (it mutates user state)."""
    if _BOUND:
        return _act("clipboard_set", str(s))
    if not _IS_WIN:
        return "[unsupported]"
    if _gated():
        return "[disarmed]"
    data = (s + "\x00").encode("utf-16-le")
    h = _kernel32.GlobalAlloc(_GMEM_MOVEABLE, len(data))
    if not h:
        return False
    dst = _kernel32.GlobalLock(h)
    if not dst:
        return False
    ctypes.memmove(dst, data, len(data))
    _kernel32.GlobalUnlock(h)
    if _user32.OpenClipboard(0) == 0:
        return False
    try:
        _user32.EmptyClipboard()
        return bool(_user32.SetClipboardData(_CF_UNICODETEXT, h))
    finally:
        _user32.CloseClipboard()


def run(cmd, wait=False):
    """Run a command line. Arbitrary process execution is the explicit RAW escape hatch."""
    if _BOUND:
        return "[requires RAW]"
    if _gated():
        return "[disarmed]"
    env = dict(os.environ)
    env.pop("NEURON_INPUT_ARMED", None)  # don't leak armed authority into children
    if _IS_WIN:
        args = ["cmd", "/C", cmd]
    else:
        args = ["sh", "-c", cmd]
    if wait:
        p = subprocess.run(args, env=env, capture_output=True, text=True)
        return (p.returncode, p.stdout)
    subprocess.Popen(args, env=env)
    return True


def focus(title):
    """Bring a window to the foreground by exact title. Arm-gated."""
    if _BOUND:
        return _act("focus", str(title))
    if not _IS_WIN:
        return "[unsupported]"
    if _gated():
        return "[disarmed]"
    h = _user32.FindWindowW(None, title)
    if not h:
        return False
    return bool(_user32.SetForegroundWindow(h))


def sleep(ms):
    """Sleep for `ms` milliseconds."""
    time.sleep(ms / 1000.0)
