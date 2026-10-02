---
name: neuron-macros
description: Author, revise, explain, and verify Neuron Python macros. Use for macro code, action-specific input and return contracts, beacons, macro composition, configurable options, and integrations with programs or APIs.
---

# Macros in Neuron

A macro is a Python file with `def macro(ctx):`. Neuron runs it in a managed
CPython worker. `ctx` carries the foreground app, window, working directory,
and clipboard snapshot supplied by its caller.

## Use the installed API

Run `neuron macro prelude` for the exact `neuron` module in the user's build.
When working in this repository, [`runtime/host/neuron.py`](../../runtime/host/neuron.py)
is the source and [the macro overview](../../docs/GDD.md#macros--beacons) explains the runtime.
Check helper names and return values against the installed prelude.
`neuron macro check <file.py>` parses a candidate without executing it.

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

## Macros for use in actions

An action-specific macro uses the same `def macro(ctx):` entry point; the
calling action defines its input and required return value. Check that
contract in the installed action catalog before writing it. Clipboard
Transform uses a saved macro as an operation in its text-processing chain.

For **Clipboard Transform**, `ctx.clipboard` is the text at that step in the
chain, including earlier transforms. The other trigger context is preserved.
Return a Python `str`; `""` is valid. Printing, typing, setting the clipboard,
or returning `None` or a dict does not supply the result. Keep the macro's
output in its return value so the action owns the clipboard update.

```python
def macro(ctx):
    text = ctx.clipboard or ""
    return " ".join(text.split())
```

Save and register it, then choose Clipboard Transform → Python macro → the
saved macro → + add macro in the action editor. CLI operation JSON is
`{"op":"macro","id":"clean_text"}` when registered as `clean_text`.
Native operations can precede or follow it; one macro operation is allowed
per chain. It requires armed input and runs asynchronously. Input and output
are limited to 1 MiB of UTF-8 text, with a five-minute result wait budget.
An error or invalid result leaves the clipboard unchanged; a clipboard change
while it runs discards the result.

An LLM or decision-model integration follows the same text-in, text-out
contract. Use RAW for direct API calls, bound each request with a timeout,
validate the response, and raise on failure rather than returning empty text
as a fallback. Disarming or a transform timeout cannot cancel direct RAW
effects already running. Workshop tests and CLI runs do not sandbox those
effects. Explain the input and return contract with the user's example.

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
`neuron.ask` or `choose`, then pass the answer back. Use this pattern to
connect an agent harness to Neuron's beacons.

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
`choose`. Check the installed runner's prompt support; verify multi-option
wheels in the GUI.

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
   then bind it through Neuron's normal trigger → action path, either as a
   macro action or in an action's supported macro slot. For Clipboard
   Transform, check the returned text in Workshop, then verify the complete
   chain when the user intends a live clipboard update. Report what was
   actually exercised.

Route macros through Neuron's trigger → action pipeline. Choose helpers and
backends from the installed build, and report the behavior actually verified.
