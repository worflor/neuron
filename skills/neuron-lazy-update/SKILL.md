---
name: neuron-lazy-update
description: Operate, maintain, troubleshoot, and explain Neuron. Use for general Neuron questions, status and version checks, installation, updates, rollback, removal, and investigating problems with the app, CLI, devices, or configuration.
---

# Using and maintaining Neuron

Start with the user's goal and the installed build. Use the references below
for the relevant commands, release workflow, and product evidence.

## Pick the right file

| the user wants to… | read |
|---|---|
| install, update, roll back, uninstall, or know their version | [update.md](update.md) |
| change a setting, read a device, or run any `neuron` command | [cli.md](cli.md) |
| know how something works, or whether it's supported | [answers.md](answers.md) |
| says something didn't work, or asks why something is the way it is | [issues.md](issues.md) |
| author or revise a Python macro, beacon, or action-specific macro | [neuron-macros](../neuron-macros/SKILL.md) |
| author bindings, cast, lighting, profiles, or a complete CLI setup | [neuron-cli](../neuron-cli/SKILL.md) |

Read only the file you need.

## The loop

For questions, inspect the relevant evidence and answer. For changes:

1. **Check.** Look before touching anything: run the read-only step (`-Action check`, or a
   read-only CLI command).
2. **Report.** Tell the user what you found in one or two plain sentences, including any `FLAG:`
   lines.
3. **Establish scope.** Match the operation to the user's request. If the change needs additional
   authorization, explain the specific effect and ask. Updating closes Neuron; device writes change hardware.
4. **Act.** Run the authorized operation.
5. **Verify.** Read the result back: the script's final `RESULT:` line, or re-read the setting
   you changed.
6. **Report.** Say what happened, including anything that didn't go to plan.

If step 5 doesn't match what you expected, stop and tell the user. Don't try a different command
to force it.

## Operating constraints

1. **`RESULT: blocked`, or any `FLAG` marked `(STOP)`, means stop.** Show the user the lines. Never
   work around a checksum or attestation failure, and never retry with different flags to get past
   one.
2. **Never delete the user's config.** Portable profiles, bindings and settings sit next to the
   executables; installer-managed config sits in `%LOCALAPPDATA%\neuron`. Update and rollback may
   replace verified release payload files, but their ownership never extends to runtime config.
3. **Never do these unless the user asks for that exact thing:** arm input, use `--persist`, run
   `neuron-app.exe --purge-synapse`, pass `-AllowDowngrade`, run a macro file, or file an issue.
   Filing is an offer you make once. Only file after the user has approved the exact text.
4. **Use the installed command interface.** Read `neuron <command> --help` for exact commands and flags.
5. **Resolve uncertainty before changing the setup.** Inspect commands and read-only state first;
   ask the user for missing intent or information that affects the operation.

## Reporting what you notice

Tell the user about anything odd, even if the task succeeded: a `FLAG` line, a command that printed
a warning, a device missing from `neuron list`, a version that didn't change. One sentence each is
enough.
