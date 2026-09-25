---
name: neuron-macros
description: Create or revise Neuron Python macros, including beacons and integrations with local programs or APIs. Use when a user wants a macro triggered through Neuron, help with macro code, or a Neuron question or status prompt.
---

# Macros in Neuron

A macro is a Python file with `def macro(ctx):`. Neuron starts its CPython
worker when input is armed or an editor action needs it, then keeps it warm. `ctx` is a snapshot of the
foreground app, window, working directory and clipboard at trigger time.

## Use the installed API

Run `neuron macro prelude` for the exact `neuron` module in the user's build.
When working in this repository, [`runtime/host/neuron.py`](../../runtime/host/neuron.py)
is the source and [the macro overview](../../docs/GDD.md#macros--beacons) explains the runtime. Check helper names and return values there rather than inventing calls. `neuron macro check <file.py>` parses a candidate without executing it.

## Choose authority from the work

- **BOUND is the default.** Use it for computation and Neuron's brokered
  helpers: input, device and audio actions, `neuron.store/load`, macro
  composition, and beacons. The Rust host checks its effect gates.
- **RAW is explicit.** Put `# neuron: raw` in the leading comment header when
  the macro needs files, sockets, HTTP APIs, subprocesses, native libraries,
  or an agent harness. RAW is ordinary Python with the user's account
  authority. The input arm gate covers Neuron helpers; it does not contain
  direct RAW Python effects. BOUND cannot invoke a RAW macro.

Keep a macro BOUND when Neuron's helpers are enough. For RAW integrations,
use explicit destinations, timeouts and error handling so a missing service
does not leave a worker waiting indefinitely. Keep tokens and private data
out of `notify` and logs.

## Beacons

`neuron.ask(question, default=None, timeout=300, description="")` returns
`True`, `False`, or the default after a pass or timeout.
`neuron.choose(question, options, default=None, ...)` returns an option
string or the default. `neuron.confirm(...)` returns `True` after a flick,
or its default after a pass or timeout.
`neuron.notify(text)` posts status without waiting. These work while input
is disarmed; they do not authorize effects by themselves. Ask only when the
answer changes what the macro should do.

A RAW macro can call an external program or API, show its question through
`neuron.ask` or `choose`, then pass the answer back. This makes the macro
the bridge for an agent harness. Neuron does not yet expose a general
external endpoint that lets another process post a beacon directly.

Start with the smallest working shape:

```python
import neuron

def macro(ctx):
    if not neuron.ask("Run this action?", default=False):
        return
    neuron.notify("Action approved")
```

Replace the status with the intended effect. For an external agent, mark the
file RAW, call its CLI or API with a timeout, and map its reply to `ask` or
`choose`. The CLI test runner only accepts `y`/`n` at a beacon prompt; check
multi-option wheels in the GUI.

## Make and check

1. Pin down the trigger, inputs, desired effect, and what should happen on
   cancellation, timeout and service failure. Use module-level
   `NEURON_OPTIONS` when the user should tune values in the Workshop; read
   them with `neuron.option(key, default)`.
2. Write one small `macro(ctx)` entry point. Use `neuron.log` for diagnostic
   detail, `neuron.notify` for a short status the user should see, and
   `neuron.invoke` to compose registered macros.
3. Run `neuron macro check <file.py>`. A CLI
   `neuron macro run --file <file.py>` executes the code even without
   `--arm`; RAW calls to files, networks or programs are still live.
   Exercise those effects only when the user intended them.
4. Register with `neuron macro add <name> <file.py>` or use the Workshop,
   then bind it through Neuron's normal trigger → action path. Verify the
   response and report what was actually exercised.

Do not add a second dispatch path for a macro. If the installed build lacks
a helper or a platform backend, say so and use the supported path that fits
the user's goal.
