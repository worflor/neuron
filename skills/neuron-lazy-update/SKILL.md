---
name: neuron-lazy-update
description: Operate, maintain, troubleshoot, and explain Neuron. Use for general Neuron questions, status and version checks, installation, updates, rollback, removal, and investigating problems with the app, CLI, devices, or configuration.
---

# Using and maintaining Neuron

Start with the user's goal and the running copy. A repository build, an installed
release checkpoint and a portable copy can coexist. Use the references below
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

## Resolve the copy, then act

For installation/version work, run `-Action status -Json` to identify the running copy, startup
target and installed release. `-Action check` compares the selected copy with the latest stable
release. Other tasks start with a read-only CLI command.

Use the target established by the request and inspection. An old installation is stale unless the
user deliberately retained it. Ask only about an unclear target or an additional change. Updating
may restart the selected copy; changing which copy starts with Windows is a separate choice.

Run the requested operation and verify its final `RESULT:` or read the changed setting back.
Report the outcome and relevant flags briefly. If verification differs from the expected result,
stop and report it; do not try another command to force success.

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
6. **Keep checks read-only and small.** Use the updater's payload backup for updates; do not copy
   the entire installation or user-data tree for a version check. Do not inspect unrelated cached
   archives to establish the running version. Equal version numbers do not prove identical binaries
   or source commits. Checksum/provenance verification applies to the package being installed.

## Reporting what you notice

Tell the user about anything odd, even if the task succeeded: a `FLAG` line, a command that printed
a warning, a device missing from `neuron list`, a version that didn't change. One sentence each is
enough.
