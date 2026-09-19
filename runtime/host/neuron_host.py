# SPDX-FileCopyrightText: 2026 Woflo Labs
# SPDX-License-Identifier: GPL-3.0-or-later
# Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.
#
# neuron_host.py — the resident program inside the Macro Host sidecar.
#
# Neuron starts ONE bundled-CPython process running this script and keeps it warm: every macro is
# exec'd once (imports warmed) into a registry, and a trigger is a tiny framed message that calls
# the already-resident function. No per-press spawn, no per-press import. Real-time.
#
# THE LOAD-BEARING ISOLATION (read this before touching the I/O):
#   The control protocol must NEVER share a stream with a macro's own output. A macro that does
#   print(), or a C library that writes to stdout, would otherwise corrupt the frame stream. So at
#   startup — BEFORE any macro can run — we dup our ORIGINAL stdout (fd 1) aside as the private
#   protocol channel, then redirect fd 1 -> fd 2 so all macro stdout/stderr flows to the LOG pipe.
#   The host reads child-stdout = protocol (only ever written by _send), child-stderr = macro log.
#   Result: a macro physically cannot reach the protocol channel via normal output.
#
# Frame = [u32 little-endian length][utf-8 JSON]. One JSON object per frame, both directions.

import sys
import os
import io
import json
import queue
import struct
import threading
import traceback

_HOST_MODE = os.environ.get("NEURON_MACRO_MODE", "raw").strip().lower()
_BOUND = _HOST_MODE == "bound"

# ── isolate the protocol channel from macro output (must happen first) ──────────────────────────
_PROTO = os.fdopen(os.dup(1), "wb", buffering=0)   # our private copy of the real stdout = protocol
_IN = os.fdopen(os.dup(0), "rb", buffering=0)       # the real stdin = host -> sidecar control
os.dup2(2, 1)                                        # fd 1 now points at the LOG pipe (stderr)
try:
    sys.stdout = os.fdopen(1, "w", buffering=1, closefd=False)  # python-level prints -> log pipe
except Exception:
    sys.stdout = sys.stderr

_send_lock = threading.Lock()


def _send(obj):
    """Frame one JSON object onto the private protocol channel. The ONLY writer of _PROTO."""
    data = json.dumps(obj, ensure_ascii=False).encode("utf-8", "replace")
    with _send_lock:
        _PROTO.write(struct.pack("<I", len(data)))
        _PROTO.write(data)
        _PROTO.flush()


def _readn(n):
    """Read exactly n bytes from the control channel, or None on EOF (host gone)."""
    buf = bytearray()
    while len(buf) < n:
        chunk = _IN.read(n - len(buf))
        if not chunk:
            return None
        buf += chunk
    return bytes(buf)


def _read_frame():
    hdr = _readn(4)
    if hdr is None:
        return None
    (n,) = struct.unpack("<I", hdr)
    if n > (16 << 20):  # 16 MB sanity cap so a desync can't allocate the world
        return None
    body = _readn(n)
    if body is None:
        return None
    try:
        return json.loads(body.decode("utf-8", "replace"))
    except Exception:
        return {"t": "__bad__"}


# ── the host helper module every macro gets ('import neuron' + injected globals) ────────────────
import neuron as _nh  # noqa: E402  (after the fd dance, intentionally)

_nh._set_host_send(_send)  # give the beacon layer (ask/notify) the framed protocol writer

_macros = {}   # id -> callable(ctx)
_gens = {}     # id -> active source generation
_staged = {}   # token -> (id, callable, options, generation), invisible until commit
_errors = {}   # id -> last register/compile traceback (shown verbatim; never faked)


def _compile_candidate(mid, source):
    """Compile + execute one candidate namespace without mutating the live registry."""
    # Notepad/PowerShell save UTF-8 WITH a BOM; compile() rejects U+FEFF as a stray
    # non-printable. A macro must never fail for how an editor chose to save it.
    source = source.lstrip("\ufeff")
    g = {"__name__": "neuron_macro_%s" % mid, "neuron": _nh}
    # inject the helper surface as bare globals too, so a macro can call clipboard_set(...)
    # without the `neuron.` prefix (both styles work). `ctx` is EXCLUDED: snapshotting it at
    # register time would freeze a stale world — macros read the live one via the function
    # argument (`def macro(ctx):`) or `neuron.ctx` (per-thread, resolved at fire time).
    for name in _nh.__all__:
        if name == "ctx":
            continue
        g[name] = getattr(_nh, name)
    exec(compile(source, "<macro %s>" % mid, "exec"), g)
    fn = g.get("macro") or g.get("main")
    if not callable(fn):
        raise ValueError("a macro must define `def macro(ctx):` (or `def main(ctx):`)")
    return fn, _read_options(g.get("NEURON_OPTIONS"))


def _register(mid, source, generation=0):
    """Register durable source immediately (startup/respawn path)."""
    try:
        fn, opts = _compile_candidate(mid, source)
        _macros[mid] = fn
        _opts[mid] = opts
        _gens[mid] = int(generation or 0)
        _errors.pop(mid, None)
        return True, opts
    except BaseException:
        tb = traceback.format_exc()
        _errors[mid] = tb
        return False, tb


def _prepare_register(token, mid, source, generation):
    """Compile a candidate but do not make it callable yet."""
    try:
        fn, opts = _compile_candidate(mid, source)
        _staged[token] = (mid, fn, opts, int(generation or 0))
        return True, opts
    except BaseException:
        tb = traceback.format_exc()
        _errors[mid] = tb
        _staged.pop(token, None)
        return False, tb


def _commit_register(token):
    item = _staged.pop(token, None)
    if item is None:
        return False, "prepared macro no longer exists"
    mid, fn, opts, generation = item
    _macros[mid] = fn
    _opts[mid] = opts
    _gens[mid] = generation
    _errors.pop(mid, None)
    return True, None


def _discard_register(token):
    _staged.pop(token, None)


_opts = {}  # id -> the macro's declared option manifest (a sanitized list of dicts)


def _read_options(raw):
    """Sanitize a macro's `NEURON_OPTIONS` into a manifest the host can trust (and render). A
    malformed declaration degrades to nothing rather than breaking registration."""
    out = []
    if not isinstance(raw, (list, tuple)):
        return out
    allowed = {"string", "text", "choice", "number", "bool"}
    for item in raw:
        if not isinstance(item, dict):
            continue
        key = item.get("key")
        if not isinstance(key, str) or not key:
            continue
        typ = str(item.get("type", "string")).lower()
        if typ not in allowed:
            typ = "string"
        opt = {
            "key": key,
            "label": str(item.get("label", key)),
            "type": typ,
            "default": item.get("default", "" if typ in ("string", "text") else None),
        }
        if typ == "choice":
            ch = item.get("choices") or []
            opt["choices"] = [str(c) for c in ch] if isinstance(ch, (list, tuple)) else []
        if typ == "number":
            for k in ("min", "max", "step"):
                if isinstance(item.get(k), (int, float)):
                    opt[k] = item[k]
        out.append(opt)
    return out


def _check(source):
    """Syntax + top-level def/class names. ast.parse only — no exec, zero side effects."""
    import ast
    try:
        tree = ast.parse(source.lstrip("\ufeff"))  # same editor-BOM tolerance as _register
        defs = [n.name for n in tree.body
                if isinstance(n, (ast.FunctionDef, ast.AsyncFunctionDef, ast.ClassDef))]
        has_entry = any(d in ("macro", "main") for d in defs)
        return True, defs, has_entry, None
    except SyntaxError:
        return False, [], False, traceback.format_exc()


# -- bidirectional macro model: Python source -> a typed NODE TREE (the visual-constructor model) --
# We own the real Python grammar here via `ast`; the Rust side never re-implements parsing. The JSON
# shape MUST match neuron-core's MacroNode (serde tag = "kind", snake_case). Anything we don't model
# becomes a verbatim {"kind":"raw","code": <unparsed source>} node -- lossless, so a half-written or
# arbitrarily-complex macro still maps to a tree and round-trips. The inverse (nodes -> source) lives
# in Rust (node.rs::nodes_to_source); together they make the mapping two-way.

def _called_name(call):
    """The helper name a Call targets, accepting BOTH `neuron.foo(...)` and bare `foo(...)`.
    Returns the function name str, or None if it isn't a plain name / `neuron.`-attribute call."""
    import ast
    fn = call.func
    if isinstance(fn, ast.Name):                 # foo(...)
        return fn.id
    if isinstance(fn, ast.Attribute) and isinstance(fn.value, ast.Name) \
            and fn.value.id == "neuron":         # neuron.foo(...)
        return fn.attr
    return None


def _str_literal(node):
    """If `node` is a string-literal Constant, its value; else None (so a chord/key/button arg that
    isn't a plain string falls back to raw -- those fields are key NAMES, not Values)."""
    import ast
    if isinstance(node, ast.Constant) and isinstance(node.value, str):
        return node.value
    return None


# ── the VALUE parser: an `ast` expression -> the Value JSON (the inverse of Rust's value_to_source).
# Climbs the same ladder the Rust enum does: literals -> ctx read -> name -> transform call -> binary
# expr -> the {"v":"raw"} escape hatch (ast.unparse of anything else). So EVERY parameter is modelled
# as an expression tree, never flattened to a string -- that is what makes a macro "not limiting".
def _parse_value(node):
    import ast
    # literals: str / int / bool. (A bool is an int subclass in Python -> test bool FIRST.)
    if isinstance(node, ast.Constant):
        v = node.value
        if isinstance(v, bool):
            return {"v": "bool", "b": v}
        if isinstance(v, int):
            return {"v": "int", "n": v}
        if isinstance(v, str):
            return {"v": "str", "s": v}
        # None / float / bytes etc. -> raw (keeps the exact literal)
        return {"v": "raw", "expr": ast.unparse(node)}
    # a negative int literal parses as UnaryOp(USub, Constant) -> fold to a signed Int so it
    # round-trips through Rust's i64 (value_to_source emits "-3", which re-parses to this).
    if isinstance(node, ast.UnaryOp) and isinstance(node.op, ast.USub) \
            and isinstance(node.operand, ast.Constant) and isinstance(node.operand.value, int) \
            and not isinstance(node.operand.value, bool):
        return {"v": "int", "n": -node.operand.value}
    # ctx.<field>  (read of the captured world)
    if isinstance(node, ast.Attribute) and isinstance(node.value, ast.Name) \
            and node.value.id == "ctx":
        return {"v": "ctx", "field": node.attr}
    # a bare name -> Var
    if isinstance(node, ast.Name):
        return {"v": "var", "name": node.id}
    # recv.method(args) -> Call, but ONLY when the receiver is itself a parseable Value (a transform
    # chain like ctx.selection.upper()). A call whose func isn't an Attribute, or whose receiver
    # we can't model, stays raw -- we never half-model a value.
    if isinstance(node, ast.Call) and isinstance(node.func, ast.Attribute) \
            and not node.keywords and not _has_starred(node.args):
        recv = _parse_value(node.func.value)
        return {
            "v": "call",
            "recv": recv,
            "method": node.func.attr,
            "args": [_parse_value(a) for a in node.args],
        }
    # left op right -> Bin (arithmetic). The op is the python source token.
    if isinstance(node, ast.BinOp):
        op = _BINOP.get(type(node.op))
        if op is not None:
            return {"v": "bin", "op": op,
                    "left": _parse_value(node.left), "right": _parse_value(node.right)}
    # a SINGLE comparison (a == b) -> Bin. A chained compare (a < b < c) has >1 op -> raw.
    if isinstance(node, ast.Compare) and len(node.ops) == 1:
        op = _CMPOP.get(type(node.ops[0]))
        if op is not None:
            return {"v": "bin", "op": op,
                    "left": _parse_value(node.left), "right": _parse_value(node.comparators[0])}
    # a boolean op (and/or), possibly n-ary -> fold LEFT into nested Bins so it round-trips.
    if isinstance(node, ast.BoolOp):
        op = "and" if isinstance(node.op, ast.And) else "or"
        acc = _parse_value(node.values[0])
        for v in node.values[1:]:
            acc = {"v": "bin", "op": op, "left": acc, "right": _parse_value(v)}
        return acc
    # everything else -> the verbatim escape hatch.
    return {"v": "raw", "expr": ast.unparse(node)}


def _has_starred(args):
    import ast
    return any(isinstance(a, ast.Starred) for a in args)


def _binop_table():
    import ast
    return {
        ast.Add: "+", ast.Sub: "-", ast.Mult: "*", ast.Div: "/",
        ast.FloorDiv: "//", ast.Mod: "%", ast.Pow: "**",
    }


def _cmpop_table():
    import ast
    return {
        ast.Eq: "==", ast.NotEq: "!=", ast.Lt: "<", ast.LtE: "<=",
        ast.Gt: ">", ast.GtE: ">=", ast.In: "in", ast.NotIn: "not in",
        ast.Is: "is", ast.IsNot: "is not",
    }


_BINOP = _binop_table()
_CMPOP = _cmpop_table()


def _is_paste(call):
    """neuron.hotkey("ctrl", "v") -> the special-cased Paste node (recognised both ways)."""
    if _called_name(call) != "hotkey" or call.keywords or len(call.args) != 2:
        return False
    keys = [_str_literal(a) for a in call.args]
    return keys == ["ctrl", "v"]


def _node_for_stmt(stmt):
    """Map ONE statement to a node dict. Recognised helper calls become ACTION nodes (their data args
    parsed as Values); control statements become FLOW nodes (recursing their bodies); everything else
    becomes a verbatim raw node via ast.unparse -- one raw node per top-level statement."""
    import ast

    # ── flow: if / for / while / try / return / assignment ───────────────────────────────────────
    if isinstance(stmt, ast.If):
        # `if neuron.ask(q, description=d): <yes> [else: <no>]` -> Ask
        if isinstance(stmt.test, ast.Call) and _called_name(stmt.test) == "ask" \
                and stmt.test.args:
            describe = None
            for kw in stmt.test.keywords:
                if kw.arg == "description":
                    describe = _parse_value(kw.value)
                elif kw.arg is not None:
                    describe = "__bail__"  # an unmodelled kw (default=/timeout=) -> fall to raw `if`
            # only model when there are no OTHER positional args + no unmodelled keywords
            if len(stmt.test.args) == 1 and describe != "__bail__" \
                    and all(kw.arg == "description" for kw in stmt.test.keywords):
                return {
                    "kind": "ask",
                    "question": _parse_value(stmt.test.args[0]),
                    "description": describe if describe is not None else {"v": "str", "s": ""},
                    "yes": [_node_for_stmt(s) for s in stmt.body],
                    "no": [_node_for_stmt(s) for s in stmt.orelse],
                }
        # a plain `if cond:` -> If. An `elif` chain shows up as a single If in orelse; that nests
        # fine (the orelse is one If node). We KEEP it modelled (the inner If becomes the else_).
        return {
            "kind": "if",
            "cond": _parse_value(stmt.test),
            "then_": [_node_for_stmt(s) for s in stmt.body],
            "else_": [_node_for_stmt(s) for s in stmt.orelse],
        }

    if isinstance(stmt, ast.For) and not stmt.orelse:
        # `for _ in range(n):` -> RepeatN
        tgt = stmt.target
        it = stmt.iter
        if isinstance(tgt, ast.Name) and tgt.id == "_" and isinstance(it, ast.Call) \
                and isinstance(it.func, ast.Name) and it.func.id == "range" \
                and len(it.args) == 1 and not it.keywords:
            return {
                "kind": "repeat_n",
                "count": _parse_value(it.args[0]),
                "body": [_node_for_stmt(s) for s in stmt.body],
            }
        # `for var in <iter>:` -> ForEach (var is a plain name)
        if isinstance(tgt, ast.Name):
            return {
                "kind": "for_each",
                "var": tgt.id,
                "source": _parse_value(it),
                "body": [_node_for_stmt(s) for s in stmt.body],
            }

    if isinstance(stmt, ast.While) and not stmt.orelse:
        return {
            "kind": "repeat_while",
            "cond": _parse_value(stmt.test),
            "body": [_node_for_stmt(s) for s in stmt.body],
        }

    if isinstance(stmt, ast.Try) and not stmt.orelse and not stmt.finalbody \
            and len(stmt.handlers) == 1:
        h = stmt.handlers[0]
        # a bare `except:` or `except Exception:` (no `as name`, no other types) -> Try
        bare = h.type is None
        plain = isinstance(h.type, ast.Name) and h.type.id == "Exception"
        if (bare or plain) and h.name is None:
            return {
                "kind": "try",
                "body": [_node_for_stmt(s) for s in stmt.body],
                "except_": [_node_for_stmt(s) for s in h.body],
            }

    # bare `return` (no value) -> Stop. `return <expr>` stays raw (we model only the early-stop).
    if isinstance(stmt, ast.Return) and stmt.value is None:
        return {"kind": "stop"}

    # `name = neuron.run(x, wait=True)` -> Open{capture}; any other `name = <expr>` -> SetVar
    if isinstance(stmt, ast.Assign) and len(stmt.targets) == 1 \
            and isinstance(stmt.targets[0], ast.Name):
        name = stmt.targets[0].id
        val = stmt.value
        if isinstance(val, ast.Call) and _called_name(val) == "run" and len(val.args) == 1:
            waits = any(kw.arg == "wait" and isinstance(kw.value, ast.Constant)
                        and kw.value.value is True for kw in val.keywords)
            only_wait = all(kw.arg == "wait" for kw in val.keywords)
            if waits and only_wait:
                return {"kind": "open", "command": _parse_value(val.args[0]), "capture": name}
        return {"kind": "set_var", "name": name, "value": _parse_value(val)}

    # ── actions: an expression statement that is a single helper call ──────────────────────────────
    if isinstance(stmt, ast.Expr) and isinstance(stmt.value, ast.Call):
        call = stmt.value
        name = _called_name(call)

        # paste is hotkey("ctrl","v") -- check BEFORE the generic hotkey/press mapping.
        if _is_paste(call):
            return {"kind": "paste"}

        if name == "type_text" and len(call.args) == 1 and not call.keywords:
            return {"kind": "type", "text": _parse_value(call.args[0]),
                    "ghost": False, "speed": None}
        if name == "type_ghost" and call.args and not call.keywords:
            speed = None
            if len(call.args) >= 2:
                speed = _str_literal(call.args[1])
            if len(call.args) <= 2:
                return {"kind": "type", "text": _parse_value(call.args[0]),
                        "ghost": True, "speed": speed}
        if name == "hotkey" and call.args and not call.keywords:
            keys = [_str_literal(a) for a in call.args]
            if all(k is not None for k in keys):
                return {"kind": "press", "keys": keys}
        if name == "key" and len(call.args) == 1 and not call.keywords:
            k = _str_literal(call.args[0])
            if k is not None:
                return {"kind": "key_press", "name": k}
        if name == "click" and len(call.args) == 1 and not call.keywords:
            b = _str_literal(call.args[0])
            if b is not None:
                return {"kind": "click", "button": b}
        if name == "scroll" and len(call.args) == 1 and not call.keywords:
            return {"kind": "scroll", "amount": _parse_value(call.args[0])}
        if name == "mouse_to" and len(call.args) == 2 and not call.keywords:
            return {"kind": "move_to", "x": _parse_value(call.args[0]),
                    "y": _parse_value(call.args[1])}
        if name == "clipboard_set" and len(call.args) == 1 and not call.keywords:
            return {"kind": "copy", "text": _parse_value(call.args[0])}
        if name == "run" and len(call.args) == 1 and not call.keywords:
            return {"kind": "open", "command": _parse_value(call.args[0]), "capture": None}
        if name == "focus" and len(call.args) == 1 and not call.keywords:
            return {"kind": "focus", "window": _parse_value(call.args[0])}
        if name == "sleep" and len(call.args) == 1 and not call.keywords:
            return {"kind": "wait", "ms": _parse_value(call.args[0])}
        if name == "notify" and len(call.args) == 1 and not call.keywords:
            return {"kind": "notify", "text": _parse_value(call.args[0])}

    # anything we don't model -> verbatim source (Python 3.9+: ast.unparse)
    return {"kind": "raw", "code": ast.unparse(stmt)}


def _parse_nodes(source):
    """Python source -> typed entry body plus exact module/entry source around it."""
    import ast
    try:
        tree = ast.parse(source.lstrip("\ufeff"))
    except SyntaxError as e:
        return {"ok": False, "error": {"line": e.lineno, "msg": str(e.msg)}}

    body = tree.body
    prefix = ""
    header = "def macro(ctx):\n"
    suffix = ""
    entry = None
    for n in tree.body:
        if isinstance(n, (ast.FunctionDef, ast.AsyncFunctionDef)) and n.name in ("macro", "main"):
            entry = n
            body = n.body
            break

    if entry is not None:
        lines = source.splitlines(keepends=True)
        starts = [entry.lineno] + [d.lineno for d in entry.decorator_list]
        start = max(0, min(starts) - 1)
        end = entry.end_lineno or entry.lineno
        prefix = "".join(lines[:start])
        if body and body[0].lineno > entry.lineno:
            body_start = body[0].lineno - 1
            header = "".join(lines[start:body_start])
        elif not body:
            header = "".join(lines[start:end])
            if not header.endswith("\n"):
                header += "\n"
        else:
            header = "def macro(ctx):\n"
        suffix = "".join(lines[end:])

    return {
        "ok": True,
        "nodes": [_node_for_stmt(s) for s in body],
        "prefix": prefix,
        "header": header,
        "suffix": suffix,
    }


# ── concurrent fires: one SERIAL worker per macro id ─────────────────────────────────────────────
# Spamming a macro queues its fires IN ORDER on its own thread, while DIFFERENT macros run
# concurrently — so a macro blocked on neuron.ask() (or a slow API call) never stalls anyone else,
# and a double-press of one macro can never execute out of order. Each fire's stdout is captured
# per-THREAD (the dispatcher below), so concurrent prints can't bleed into each other's results.

class _ThreadStdout(io.TextIOBase):
    """sys.stdout dispatcher: writes go to this thread's capture buffer if a fire set one,
    else to the log pipe. Installed ONCE; immune to concurrent global stdout swaps."""

    def __init__(self, fallback):
        self._fallback = fallback
        self._local = threading.local()

    def write(self, s):
        t = getattr(self._local, "target", None)
        return (t if t is not None else self._fallback).write(s)

    def flush(self):
        t = getattr(self._local, "target", None)
        try:
            (t if t is not None else self._fallback).flush()
        except Exception:
            pass

    def writable(self):
        return True


_stdout_mux = _ThreadStdout(sys.stdout)
sys.stdout = _stdout_mux

_fire_queues = {}   # mid -> queue.Queue of fire msgs (one serial worker each)
_fire_lock = threading.Lock()
_FIRE_STOP = object()
# A macro fired faster than it runs can't grow its backlog without bound: the per-macro queue is
# capped, and a full queue DROPS THE NEWEST fire (with a log line) rather than blocking the protocol
# loop — a blocked loop would deafen the whole sidecar.
_FIRE_QUEUE_MAX = 256
# Per-id caps are not a process cap: many distinct ids could each create a worker + 256-item queue.
# Bound BOTH dimensions globally so a generated macro storm cannot turn variety into unbounded threads
# or memory. Slots are acquired before enqueue/worker creation and released on finish/refusal/retire.
_FIRE_TOTAL_MAX = 1024
_FIRE_WORKER_MAX = 128
_fire_slots = threading.BoundedSemaphore(_FIRE_TOTAL_MAX)
_fire_worker_slots = threading.BoundedSemaphore(_FIRE_WORKER_MAX)
# Editor/CLI source tests run outside the registry so they can never persist or replace a live macro.
# Bound their concurrency: a candidate can deliberately loop forever, but it can consume at most this
# many daemon workers before further tests are refused instead of growing threads without limit.
_EPHEMERAL_MAX = 4
_ephemeral_slots = threading.BoundedSemaphore(_EPHEMERAL_MAX)


def _fire_callable(mid, fn, ctx, opts, mock=False):
    _nh._set_ctx(ctx)
    _nh._set_mid(mid)
    _nh._set_options(opts)
    _nh._set_mock(mock)   # set FRESH every fire so a real fire never inherits a prior test's mock
    cap = io.StringIO()
    _stdout_mux._local.target = cap
    try:
        rv = fn(_nh._get_ctx())
        out = cap.getvalue()
        tail = out[-4096:] if out else None
        return True, (str(rv) if rv is not None else None), tail
    except BaseException:
        return False, None, traceback.format_exc()
    finally:
        _stdout_mux._local.target = None


def _fire(mid, generation, ctx, opts, mock=False):
    fn = _macros.get(mid)
    if fn is None:
        return False, None, _errors.get(mid, "macro '%s' not registered" % mid)
    active = _gens.get(mid, 0)
    if generation is not None and int(generation) != active:
        return False, None, (
            "macro '%s' fire was queued for generation %s; active generation is %s"
            % (mid, generation, active)
        )
    return _fire_callable(mid, fn, ctx, opts, mock)


def _fire_worker(mid, q):
    """One macro's serial fire loop: in-order execution, results framed back as they finish."""
    try:
        while True:
            msg = q.get()
            if msg is _FIRE_STOP:
                return
            try:
                ok, val, log = _fire(
                    mid,
                    msg.get("generation"),
                    msg.get("ctx") or {},
                    msg.get("options") or {},
                    bool(msg.get("mock")),
                )
                _send({"t": "result", "rid": msg.get("rid"), "ok": ok,
                       "value": val, "error": (None if ok else log), "log": (log if ok else None)})
            finally:
                _fire_slots.release()
    finally:
        _fire_worker_slots.release()


def _dispatch_fire(msg):
    mid = msg.get("id")
    if _macros.get(mid) is None:
        _send({"t": "result", "rid": msg.get("rid"), "ok": False, "value": None,
               "error": _errors.get(mid, "macro '%s' not registered" % mid), "log": None})
        return
    if msg.get("generation") is None:
        msg["generation"] = _gens.get(mid, 0)
    if not _fire_slots.acquire(blocking=False):
        _send({"t": "result", "rid": msg.get("rid"), "ok": False, "value": None,
               "error": "macro fire budget full (%d total)" % _FIRE_TOTAL_MAX, "log": None})
        return
    with _fire_lock:
        q = _fire_queues.get(mid)
        if q is None:
            if not _fire_worker_slots.acquire(blocking=False):
                _fire_slots.release()
                _send({"t": "result", "rid": msg.get("rid"), "ok": False, "value": None,
                       "error": "macro worker budget full (%d ids)" % _FIRE_WORKER_MAX, "log": None})
                return
            q = queue.Queue(maxsize=_FIRE_QUEUE_MAX)
            _fire_queues[mid] = q
            try:
                threading.Thread(target=_fire_worker, args=(mid, q),
                                 name="fire-%s" % mid, daemon=True).start()
            except BaseException:
                _fire_queues.pop(mid, None)
                _fire_worker_slots.release()
                _fire_slots.release()
                raise
    try:
        q.put_nowait(msg)
    except queue.Full:
        _fire_slots.release()
        sys.stderr.write("[neuron] macro '%s' fire queue full (%d) — dropped a fire\n"
                         % (mid, _FIRE_QUEUE_MAX))


def _retire_fire_worker(mid):
    """Discard queued work and retire this id's serial worker after unregister."""
    with _fire_lock:
        q = _fire_queues.pop(mid, None)
        if q is None:
            return
        while True:
            try:
                item = q.get_nowait()
            except queue.Empty:
                break
            if item is not _FIRE_STOP:
                _fire_slots.release()
        q.put_nowait(_FIRE_STOP)


def _fire_source(msg):
    """Compile + run one non-persistent source candidate on a bounded disposable worker."""
    try:
        mid = msg.get("id") or "__source__"
        try:
            fn, _ = _compile_candidate(mid, msg.get("source") or "")
            ok, val, log = _fire_callable(
                mid,
                fn,
                msg.get("ctx") or {},
                msg.get("options") or {},
                bool(msg.get("mock")),
            )
        except BaseException:
            ok, val, log = False, None, traceback.format_exc()
        _send({"t": "result", "rid": msg.get("rid"), "ok": ok,
               "value": val, "error": (None if ok else log), "log": (log if ok else None)})
    finally:
        _ephemeral_slots.release()


def _dispatch_source(msg):
    if not _ephemeral_slots.acquire(blocking=False):
        _send({"t": "result", "rid": msg.get("rid"), "ok": False, "value": None,
               "error": "too many source tests are still running", "log": None})
        return
    try:
        threading.Thread(
            target=_fire_source,
            args=(msg,),
            name="source-%s" % (msg.get("id") or "test"),
            daemon=True,
        ).start()
    except BaseException:
        _ephemeral_slots.release()
        raise


# give the helper layer its two cross-macro hooks, now that the registry + dispatcher both exist:
_nh._set_host_lookup(_macros.get)        # neuron.invoke(): resolve a sibling macro fn by id
_nh._set_host_dispatch(_dispatch_fire)   # neuron.invoke(wait=False): enqueue on the target's worker


def main():
    _send({"t": "ready", "py": sys.version.split()[0], "platform": sys.platform,
           "mode": _HOST_MODE})
    while True:
        try:
            msg = _read_frame()
        except Exception:
            break
        if msg is None:
            break
        t = msg.get("t")
        if t == "fire":
            _dispatch_fire(msg)
        elif t == "fire_source":
            _dispatch_source(msg)
        elif t == "answer":
            # the user's (or host's) pick on an open prompt — wake the waiting macro.
            _nh._deliver_answer(msg.get("pid"), msg.get("choice"))
        elif t == "act_result":
            # a device verb (neuron.dpi/profile/…) finished on the host — wake the waiting helper.
            _nh._deliver_act(msg.get("rid"), msg.get("ok"), msg.get("msg"))
        elif t == "register":
            ok, extra = _register(
                msg.get("id"), msg.get("source") or "", msg.get("generation") or 0
            )
            # on success `extra` is the declared option manifest; on failure it's the traceback.
            _send({"t": "registered", "rid": msg.get("rid"), "id": msg.get("id"), "ok": ok,
                   "error": (None if ok else extra), "options": (extra if ok else None)})
        elif t == "prepare_register":
            token = msg.get("token")
            ok, extra = _prepare_register(
                token, msg.get("id"), msg.get("source") or "", msg.get("generation") or 0
            )
            _send({"t": "prepared", "rid": msg.get("rid"), "ok": ok,
                   "error": (None if ok else extra), "options": (extra if ok else None)})
        elif t == "commit_register":
            ok, err = _commit_register(msg.get("token"))
            _send({"t": "committed", "rid": msg.get("rid"), "ok": ok, "error": err})
        elif t == "discard_register":
            _discard_register(msg.get("token"))
        elif t == "check":
            ok, defs, has_entry, err = _check(msg.get("source") or "")
            _send({"t": "checked", "rid": msg.get("rid"), "ok": ok,
                   "defs": defs, "has_entry": has_entry, "error": err})
        elif t == "parse":
            # source -> typed node tree (the visual macro constructor's model). Pure ast, no exec.
            res = _parse_nodes(msg.get("source") or "")
            _send({"t": "parsed", "rid": msg.get("rid"), "ok": res.get("ok", False),
                   "nodes": res.get("nodes"), "error": res.get("error")})
        elif t == "armed":
            _nh._set_armed(bool(msg.get("on")))
        elif t == "unregister":
            mid = msg.get("id")
            _macros.pop(mid, None)
            _gens.pop(mid, None)
            _errors.pop(mid, None)
            _opts.pop(mid, None)
            for token, item in list(_staged.items()):
                if item[0] == mid:
                    _staged.pop(token, None)
            _retire_fire_worker(mid)
        elif t == "ping":
            _send({"t": "pong", "rid": msg.get("rid")})
        elif t == "shutdown":
            break
        # unknown / __bad__ frames are ignored (forward-compatible)


if __name__ == "__main__":
    try:
        main()
    except BaseException:
        # never crash silently — surface to the log channel, then exit so the host respawns.
        try:
            sys.stderr.write(traceback.format_exc())
        except Exception:
            pass
