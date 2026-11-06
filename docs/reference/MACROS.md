# Macro API reference

> Written with LLM assistance from the module source. Check helper names and return values against
> `neuron macro prelude` for your build.

A Neuron macro is a Python file with one entry point:

```python
def macro(ctx):
    ...
```

Neuron runs it in its bundled CPython Macro Host when its trigger fires. This page covers the
`neuron` module the macro imports. The source of truth is your installed build:
`neuron macro prelude` prints the exact module, and in this repository it is
[`runtime/host/neuron.py`](../../runtime/host/neuron.py). For how the runtime works and why, see
[macros & beacons](../GDD.md#macros--beacons) in the GDD.

## Authority: BOUND and RAW

New macros run **BOUND**. Ordinary Python computation works, and machine effects (input, device
settings, audio, the store) go through the Rust host, which checks the arm gate again before acting.
Files, processes, sockets and native libraries are not part of BOUND's surface.

A macro whose leading comment header contains this line runs **RAW**:

```python
# neuron: raw
```

RAW is ordinary CPython with your account's authority. The `neuron` helpers still check the arm gate,
but code that bypasses them (`subprocess`, `ctypes`, sockets) is not gated. A BOUND macro cannot
invoke a RAW one. `neuron macro mode NAME raw|bound` changes the mode, and so does the Workshop's
RAW toggle; both edit that header line.

## Return values and the arm gate

Helpers marked **gated** do nothing while input is disarmed (SAFE mode, or a Workshop test run).
Every effect helper returns the same shapes in BOUND and RAW:

- **It happened:** `True` for input helpers, or the host's confirmation string for device, audio,
  OBS and signal helpers, such as `"DPI -> 1600"`.
- **It did not:** a string in brackets giving the reason, such as `"[disarmed]"`, `"[unsupported]"`,
  `"[unknown key 'x']"`, `"[no window titled 'x']"`, `"[timed out]"` or
  `"[brightness failed: …]"`. This string is falsy, so a plain `if` tests whether the effect
  happened, and it still compares equal to its text.

```python
import neuron

def macro(ctx):
    result = neuron.dpi(800)
    if not result:
        neuron.notify("DPI unchanged: " + result)
```

Reads (`battery`, `current_dpi`, the store, `clipboard_get`, …) and beacons (`ask`, `notify`, …) are
never gated. `neuron.armed()` reports whether gated helpers would act right now.

## `ctx`: where you were when it fired

`ctx` is a snapshot taken once when the trigger fires, so the whole run sees one consistent moment.
`neuron.ctx` is the same object.

| field | value |
|---|---|
| `ctx.app` | foreground executable name, lowercased (`"code.exe"`), or `None` |
| `ctx.title` | foreground window title, or `None` |
| `ctx.cwd` | the folder Explorer is showing or a terminal's working directory, best effort; often `None` |
| `ctx.clipboard` | clipboard text, or `None` if empty or not text |
| `ctx.selection` | `None` on a real trigger: reading the selection would mean sending Copy and changing your clipboard. Only a synthetic context, such as a test, supplies it |
| `ctx.prev_window` | the handle of the window that had focus, `0` if unknown |
| `ctx.armed` | informational only; use `neuron.armed()` to decide |

```python
import neuron

def macro(ctx):
    if ctx.app == "explorer.exe" and ctx.cwd:
        neuron.notify("Explorer is in " + ctx.cwd)
    elif ctx.clipboard:
        neuron.type_text(ctx.clipboard.strip())
```

## Input

All gated. Key names are names like `"f"`, `"enter"`, `"f5"`, `"ctrl"`, `"shift"`.

| helper | does |
|---|---|
| `key(name)` | press and release one key |
| `key_down(name)`, `key_up(name)` | hold and release, for chords you time yourself |
| `hotkey(*names)` | a chord: down in order, up in reverse, e.g. `hotkey("ctrl", "shift", "v")` |
| `type_text(s)` | type a Unicode string at once |
| `type_ghost(s, speed="borderline")` | type with a human cadence; `speed` is `"instant"`, `"borderline"`, `"fast"` or `"normal"` (about 165 ms a character) |
| `mouse_move(dx, dy)` | move the cursor by a relative delta |
| `mouse_to(x, y)` | move the cursor to a pixel on the primary screen |
| `click(button="left")` | `"left"`, `"right"` or `"middle"` |
| `scroll(notches)` | positive scrolls up, away from you |
| `focus(title)` | bring the window with exactly this title to the front |
| `clipboard_set(s)` | put text on the clipboard |
| `clipboard_get()` | read clipboard text, or `None`; not gated |
| `sleep(ms)` | wait; not gated |
| `run(cmd, wait=False)` | RAW only (BOUND returns `"[requires RAW]"`). Runs a shell command; with `wait=True` returns `(exit_code, stdout)` |

```python
import neuron

def macro(ctx):
    neuron.hotkey("ctrl", "l")
    neuron.sleep(50)
    neuron.type_ghost("https://example.com", speed="fast")
    neuron.key("enter")
```

## Beacons: `ask`, `choose`, `confirm`, `notify`, `log`

A beacon pauses only this macro and shows a line at the top of your monitor. Hold the cast trigger
and an answer wheel opens under the cursor. Beacons synthesize no input, so they work while input is
disarmed, and they never raise. Other macros keep firing while one waits. On the CLI the question
appears in the terminal instead.

| helper | returns |
|---|---|
| `ask(text, default=None, timeout=300, description="")` | `True` (flick west), `False` (flick east), or `default` after a pass or timeout |
| `choose(text, options, default=None, timeout=300, description="")` | the chosen option string, or `default` |
| `confirm(text, default=False, timeout=300, description="")` | `True` if you flick in any direction, otherwise `default`; a pause you can interrupt |
| `notify(text)` | posts a one-line status without waiting; `False` only when no host is connected |
| `log(*args)` | writes a line to the macro log, for diagnostics you don't need to see |

`description` is shown under the wheel. `timeout` is in seconds. Keep `choose` to a few options; the
wheel has to stay easy to hit.

```python
import neuron

def macro(ctx):
    target = neuron.choose("Deploy where?", ["staging", "production"], default=None)
    if target is None:
        return
    if target == "production" and not neuron.ask("Really production?", default=False,
                                                  description="This is the live site"):
        return
    neuron.notify("deploying to " + target)
```

## Options

Declare `NEURON_OPTIONS` at module scope and the Workshop renders a control for each one. Read the
chosen value at fire time with `option(key, default)`, or all of them with `options()`.
`neuron macro options NAME --set JSON` sets them from the CLI.

Each option has a `key`, a `label`, a `type` and a `default`. The types are `"string"`, `"text"`
(multiline), `"choice"` (needs `"choices"`), `"number"` and `"bool"`.

```python
import neuron

NEURON_OPTIONS = [
    {"key": "greeting", "label": "Greeting", "type": "string", "default": "hi"},
    {"key": "speed", "label": "Typing speed", "type": "choice",
     "choices": ["instant", "borderline", "fast", "normal"], "default": "fast"},
]

def macro(ctx):
    neuron.type_ghost(neuron.option("greeting", "hi"), speed=neuron.option("speed", "fast"))
```

## The store

Each macro has its own JSON key-value store that survives restarts. Values must be JSON-serializable.
None of these raise or need input to be armed.

| helper | does |
|---|---|
| `store(key, value)` | save a value; `True` on success |
| `load(key, default=None)` | read a value, or `default` if unset |
| `forget(key=None)` | delete one key, or with no key, this macro's whole store |
| `stored()` | a copy of the whole store as a dict |

RAW keeps it in `%LOCALAPPDATA%\neuron\macro_state\<macro id>.json` (override with
`NEURON_MACRO_STATE`). BOUND's store is owned by the host but uses the same files, so switching modes
keeps the data.

```python
import neuron

def macro(ctx):
    count = neuron.load("count", 0) + 1
    neuron.store("count", count)
    neuron.notify("fired %d times" % count)
```

## `invoke`: calling another macro

`invoke(name, wait=True, **options)` runs another registered macro by its id. The keyword arguments
become the callee's option values, and it sees the caller's `ctx`.

- With `wait=True` it returns the callee's return value. A call between macros in the same mode
  returns the Python value itself; a RAW macro calling a BOUND one gets the value back as a string.
- With `wait=False` it queues the callee on its own worker and returns `None` straight away.
- It returns `None` when the macro does not exist, raises (the traceback goes to the log), or the
  call chain is more than 16 deep.
- A BOUND macro cannot invoke a RAW one.

```python
import neuron

# registered as "clean_text"
def macro(ctx):
    return " ".join((ctx.clipboard or "").split())
```

```python
import neuron

def macro(ctx):
    cleaned = neuron.invoke("clean_text")
    if cleaned:
        neuron.clipboard_set(cleaned)
```

## Driving Neuron and your devices

These run the same actions a bound trigger would. The setters are gated and return the host's
confirmation string; the readers are not gated.

| helper | does |
|---|---|
| `dpi(value)` | set DPI, clamped to the device's range |
| `dpi_cycle(direction="up")` | next or previous DPI stage of the active profile |
| `scroll_cycle(direction="up")` | next or previous HyperScroll wheel stage |
| `profile(name)`, `profile_cycle(direction="up")` | switch or cycle saved profiles |
| `brightness(pct)` | lighting brightness, 0–100 |
| `mic_mute(mode="toggle")`, `out_mute(mode="toggle")` | `"on"`, `"off"` or `"toggle"` for the system microphone and output |
| `mic_gain(delta_pct)`, `out_gain(delta_pct)` | nudge a volume, e.g. `+5` or `-10` |
| `battery()` | `{"percent": int, "charging": bool}`, or `None` |
| `current_dpi()` | an int, or `None` |
| `active_profile()` | the active profile's name, or `""` |
| `scroll_stage()` | the HyperScroll stage (1-based), or `None` |
| `signal(channel, value=1.0)` | drive Signal lighting channel 1–4 with a value from 0 to 1; the value stays until changed; not gated |

```python
import neuron

def macro(ctx):
    b = neuron.battery()
    if b and b["percent"] < 20 and not b["charging"]:
        neuron.notify("mouse at %d%%" % b["percent"])
        neuron.signal(1, 1.0)
```

## OBS

These need OBS connected under SYSTEM → CONNECTIONS. The control helpers are gated.

| helper | does |
|---|---|
| `obs_scene(name)` | switch scene |
| `obs_stream(mode="toggle")` | `"start"`, `"stop"` or `"toggle"` |
| `obs_record(mode="toggle")` | `"start"`, `"stop"`, `"toggle"`, or `"pause"` to pause or resume |
| `obs_replay(mode="save")` | save a clip, or `"start"` / `"stop"` the replay buffer |
| `obs_mute(input_name="Mic/Aux")` | toggle mute on an OBS input |
| `obs_request(request_type, data=None)` | send any obs-websocket v5 request; the response is not returned |
| `obs_connected()`, `obs_scene_name()`, `obs_streaming()`, `obs_recording()` | read OBS state; not gated |

```python
import neuron

def macro(ctx):
    if neuron.obs_connected() and neuron.obs_streaming():
        neuron.obs_replay("save")
        neuron.notify("clipped")
```

## Checking a macro

- `neuron macro check FILE` parses it with the bundled Python without running it.
- `neuron macro run --file FILE` runs it once without storing it; input helpers are traced rather
  than fired unless you add `--arm`. RAW calls to files, the network or other programs still run.
- `neuron macro add NAME FILE` registers it, and `--action macro:NAME` binds it to a trigger.

The full CLI is in [CLI.md](CLI.md#macro). For action-specific contracts such as Clipboard
Transform, see [`skills/neuron-macros`](../../skills/neuron-macros/SKILL.md).
