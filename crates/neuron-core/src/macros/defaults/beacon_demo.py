# SPDX-FileCopyrightText: 2026 Woflo Labs
# SPDX-License-Identifier: GPL-3.0-or-later
# Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

"""A BEACON exemplar that's safe to poke at. It does the one thing neuron.ask is for: pause a running
macro and ask a yes/no with context, and nothing more. No keystrokes, no clicks, no device writes; it
just hands your answer back to the log. Read it, Test it, then copy it and slot your own work in."""
def macro(ctx):
    import neuron
    # The strip rises at your cursor. Hold the cast trigger and flick to answer (right yes, left no).
    if neuron.ask("ready?", description="a harmless demo; the answer is all it returns"):
        return "you flicked yes"   # a real macro would do its thing here instead
    return "you flicked no"
