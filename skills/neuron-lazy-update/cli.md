# Driving the neuron CLI

The CLI is `neuron.exe`, in the install folder next to `neuron-app.exe`. It uses the same engine
and the same config as the app. Run it with its full path, or from inside that folder.

On Linux it is just `neuron`, wherever the user put it, and it is the whole product there — there
is no app to sit beside. Everything below applies unchanged; drop the `.exe` and the `.\`.

## Find the right command

Don't guess command names. Ask the CLI:

```powershell
.\neuron.exe --help
.\neuron.exe dpi --help
```

Every command is top-level (`neuron dpi`, not `neuron device dpi`). A few take their own
subcommand: `lighting`, `profile`, `macro`, `audio`, `bind`, `radial`, `cast`, `gesture`, `twin`,
`prof`.

## Safe any time: read-only

These only read. Run them freely to answer questions or check things before a change.

| command | shows |
|---|---|
| `list` | connected, recognised devices |
| `info` | firmware, device mode, battery |
| `battery` | battery level and charging |
| `dpi` (no number) | current DPI |
| `polling` (no number) | current polling rate |
| `brightness` (no number) | current lighting brightness |
| `scroll` (no number) | current scroll-wheel stage |
| `lod` (no options) | current lift-off distance |
| `game-mode` (no argument) | keyboard game mode state |
| `storage` | onboard memory usage |
| `backup` | saves every device's full state to `backups\*.json` (writes a file, not the device) |
| `verify <backup.json>` | compares a device to a saved backup |
| `profile list`, `profile show <name>` | saved profiles |
| `macro list`, `audio list`, `bind list` | what's configured |

## Changes the device: confirm first

Anything given a value writes to the hardware. Before running one:

1. Read the current value first (the read-only form above), so you can tell the user what's changing.
2. Tell the user the exact command, and get a yes.
3. Run it.
4. Read the value back and report both numbers.

| command | what it changes |
|---|---|
| `dpi <n>` · `dpi-stages <…>` | DPI, or the whole DPI stage table |
| `polling <hz>` | polling rate |
| `brightness <0-100>` | lighting brightness |
| `scroll <stage>` · `lod …` | scroll stage, lift-off distance |
| `game-mode on\|off` · `remap …` · `mode …` | game mode, thumb-button remap, device control mode |
| `lighting effect …` · `profile apply <name>` | lighting, or a whole profile |

Most writes are **volatile**: they take effect now and reset when the device power-cycles.

**One exception: `scroll <stage>` stores to onboard memory by default**, matching what Synapse
sends. Tell the user that before running it, and add `--volatile` if they only want it until the
next power cycle. `dpi-stages` stores only with `--persist`. Never add `--persist` unless the user
asks for it.

A write that doesn't take prints an error, never a fake success. If you get one, report it. Don't
retry with other values.

## Leave these alone unless asked

- **`run`** starts the remap daemon. If the neuron app is already running, that means two things
  handling the same buttons. Only run it if the user asks, and suggest `run --safe` to watch without
  acting.
- **`macro run`** and any `.py` macro file run real, unsandboxed Python. Only run macros the user
  wrote or has read.
- **`probe`**, **`discover`** and **`adopt`** are for supporting new or unknown hardware. They're
  not needed for everyday use.
- **`neuron-app.exe --purge-synapse`** stops and disables Razer Synapse's services and needs admin.
  `--scan-synapse` shows what it would touch without changing anything. Run that first.

## When something doesn't work

| symptom | try |
|---|---|
| `list` shows no devices | is Razer Synapse running? neuron and Synapse can't share a device. Is the device plugged in? |
| a setting doesn't stick after unplugging | writes are volatile by default. That's expected, not a bug |
| an error you don't understand | see [answers.md](answers.md) for where to look, or the bug report steps there |
