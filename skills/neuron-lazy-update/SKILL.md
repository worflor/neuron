---
name: neuron-lazy-update
description: Install, update, roll back, or uninstall neuron (the open, no-account Razer control app - app plus CLI on Windows, CLI on Linux), drive its `neuron` CLI safely, and answer questions about it. Use when a user asks to install or update neuron, check which version they have, change a device setting through the CLI, fix a neuron problem, or asks how neuron works. Written so small models can follow it step by step. A bundled script does the risky parts and reports plain status lines.
---

# neuron: the lazy man's auto-update

This skill lets any agent install and update neuron, run its CLI, and answer questions about it,
without a frontier model and without the user knowing how any of it works.

You don't need to be clever here. Follow the steps, read the status lines the script prints, and
stop when it tells you to.

## Pick the right file

| the user wants to… | read |
|---|---|
| install, update, roll back, uninstall, or know their version | [update.md](update.md) |
| change a setting, read a device, or run any `neuron` command | [cli.md](cli.md) |
| know how something works, or whether it's supported | [answers.md](answers.md) |
| says something didn't work, or asks why something is the way it is | [issues.md](issues.md) |

Read only the file you need.

## The loop

Every task in this skill runs the same loop. Don't skip steps.

1. **Check.** Look before touching anything: run the read-only step (`-Action check`, or a
   read-only CLI command).
2. **Report.** Tell the user what you found in one or two plain sentences, including any `FLAG:`
   lines.
3. **Confirm.** Say exactly what you're about to do and get a yes. Updating closes neuron, and
   device commands change hardware.
4. **Act.** Run the one step you confirmed.
5. **Verify.** Read the result back: the script's final `RESULT:` line, or re-read the setting
   you changed.
6. **Report.** Say what happened, including anything that didn't go to plan.

If step 5 doesn't match what you expected, stop and tell the user. Don't try a different command
to force it.

## Hard rules

These are not judgement calls.

1. **`RESULT: blocked`, or any `FLAG` marked `(STOP)`, means stop.** Show the user the lines. Never
   work around a checksum or attestation failure, and never retry with different flags to get past
   one.
2. **Never delete the user's config.** Their profiles, bindings and settings sit next to the exes
   or in `%LOCALAPPDATA%\neuron`. The script never deletes anything, and neither do you.
3. **Never do these unless the user asks for that exact thing:** arm input, use `--persist`, run
   `neuron-app.exe --purge-synapse`, pass `-AllowDowngrade`, run a macro file, or file an issue.
   Filing is an offer you make once. Only file after the user has approved the exact text.
4. **Don't invent commands or flags.** If you're unsure, run `neuron.exe <command> --help` and use
   what it says.
5. **When you're unsure, ask the user.** A wrong guess here changes someone's hardware or setup.

## Reporting what you notice

Tell the user about anything odd, even if the task succeeded: a `FLAG` line, a command that printed
a warning, a device missing from `neuron list`, a version that didn't change. One sentence each is
enough. Noticing is part of the job.
