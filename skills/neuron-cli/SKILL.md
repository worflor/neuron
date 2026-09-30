---
name: neuron-cli
description: Drive Neuron end to end from the command line. Use when a user wants binds, rebinds, macros, the cast wheel, glyphs, lighting, profiles, app routing, device feel or their whole setup created, changed or inspected without the GUI, or wants a Neuron setup exported and restored.
---

# Driving Neuron through the CLI

`neuron` is the same engine as the tray app. Everything the app can author, the CLI can author, and a
running app picks the edit up within about a second. Full reference: [`docs/CLI.md`](../../docs/CLI.md).

## Rules before you touch anything

- **Never arm input.** No verb here synthesizes keystrokes. Do not pass `--arm` to `macro run` or
  `button apply` unless the user asked for that run.
- **Device writes are the user's hardware.** Reads (`control list`, `button plan`, `bind list`, `status`)
  are safe. `button apply`, `dpi`, `idle`, `profile apply` write the device; do them only when asked,
  and report the read-back the command prints. Do not enable the gated writes.
- **Rehearse in a scratch root.** `NEURON_RUN_DIR=<temp dir>` points every verb at an empty config, so
  you can try a whole setup, `dump` it, and only then `apply` it for real. With the real root, run
  `neuron dump --out backup.toml` first: it is the undo.
- **A profile called `gui` is reserved.** Binds without `--profile` live in that file on purpose.

## Work in this loop

1. **Discover.** `neuron status --json`, `neuron catalog --json` (every action, trigger, lighting preset
   and knob, emblem, preference key), `neuron control list --json` (devices and their controls),
   `neuron bind list --json`, `neuron dump`.
2. **Check before you write.** `neuron bind check --trigger T --action A`, `neuron action check SPEC`,
   `neuron trigger check SPEC` say what a spec resolves to and what is wrong with it, with no side
   effects. Every write validates too and refuses on error, so a rejected command changed nothing.
3. **Write.** Use `--json` and read the reply: it is the state that landed, re-read from disk, plus
   `"live": "reloaded"` (the app took it) or `"no-app"` (it will read it at next start).
4. **Verify.** `bind list`, `cast show`, `profile show NAME`, `bind live` (the spine as the engine
   assembles it) show the result. Do not report success from a green exit code alone on a device write:
   read the `verified` field.

## Naming a trigger and an action

Shorthand (`kind:value`), inline JSON, or an inline TOML table; all resolve to the same typed value.

```text
triggers  mouse:4  key:f13  macro:M1  input:0x09/0x04@00a8  'label:Left Ctrl'  gesture:circle
          radial:3  app:valorant  mic-tap  hold:LAYER  cast:2  game-light:APP/EFFECT
actions   key:ctrl+shift+s  dpi:1600  macro:NAME  run:CMD  mute:mic  volume:+4  profile:game
          keys:w:180 ~90 a     '{"type":"turbo","action":{"type":"key","key":"f"},"cps":12}'
```

When you do not know a control's name, ask: `neuron control list --json` names a device's controls with
ready-made `spec` strings, `neuron control catalog --json` names every key, and `neuron control capture`
waits for the user to press it. Scope a control to one device with `@pid` (`mouse:4@00a8`) whenever two
devices could send the same code (macro keys, side plates).

Any action, including ones with no shorthand (`sequence`, `script` of kind shell/file, `obs`, `summon`),
is available as JSON: `neuron action list --json` has an example of every variant to copy.

## Worked examples

**Rebind a side-plate button to a key, in firmware.** A bind on a firmware-assignable button (see
`neuron button plan`) whose action is one key or chord is performed by the device itself.

```powershell
neuron button plan --json                      # which buttons are firmware-assignable, and their pid
neuron bind add --trigger 'key:=@00a7' --action key:g --json
neuron button plan                             # the button now shows role "firmware"
neuron button apply --arm                      # only when asked: writes it, reads it back; or let the app do it
```

**A macro on a chord layer.**

```powershell
neuron macro add greet --source "def macro(ctx):`n    import neuron`n    neuron.notify('hi')`n"
neuron bind hold set --trigger mouse:5                         # the HyperShift hold key
neuron bind add --trigger key:f5 --action macro:greet --hypershift
```

**A game profile with lighting, binds, and an app route.**

```powershell
neuron profile new game
neuron profile set game dpi=1600 polling=1000 disable-win=true
neuron lighting stack add --profile game --preset fire --blend add
neuron lighting stack add --profile game --pattern uniform --color ff8800 --disable
neuron bind add --profile game --trigger mouse:4 --action dpi:800
neuron profile route add valorant game
neuron profile new everyday
neuron profile route default everyday                          # where a non-game app returns
```

**The cast wheel.**

```powershell
neuron cast set --sectors 6 --assist 0.3
neuron cast wedge set 0 --action key:1
neuron cast wedge set 1 --action curtain --hyper
neuron cast rhythm set 2 --action whiteboard                   # two taps then hold
neuron cast glyph bind circle --action mute:mic                # the glyph must be recorded first
```

**Set up a whole machine from a document.**

```powershell
$env:NEURON_RUN_DIR = "$env:TEMP\rehearsal"
neuron apply setup.toml --dry-run --json      # validates everything, lists what would change
neuron apply setup.toml --json                # writes it; a second run reports every section "unchanged"
neuron dump                                   # prints it back, byte for byte
Remove-Item Env:NEURON_RUN_DIR
neuron apply setup.toml                       # the real thing (dump backup.toml first)
```

`apply` treats each section it contains as authoritative and leaves the others alone; `--prune` also
removes profiles, macros, badges and preference keys the document omits. Add `--strict` to make a macro
or profile name that does not exist an error instead of a warning.

**Bring a Synapse 3 layout across.** Synapse 3 keeps profiles in an encrypted account cache, but its
service logs every mapping it pushed to a device. `neuron import` (no file) lists those logs; importing one
previews the user's layer as binds.

```powershell
neuron import                                                          # lists Synapse3\Log\*Mapping*.log
neuron import C:\ProgramData\Razer\Synapse3\Log\Mouse_00a8_MappingV2.log # preview: the user's binds
```

Author what it prints with `bind add` rather than `--apply`: `--apply` writes a profile whose binds are
live only while that profile is active. The log holds only what the last Synapse session wrote, so tell the
user where the layout came from and have them try it on the hardware.

**Side plates.** A plate's binds are ordinary binds, live while that plate is on. The 2-button plate
comes as Mouse 4 / Mouse 5 (the device file's `side_plate_binds`); a bind of the user's own on either
button replaces that. Plates reuse thumb-grid ids (the 2-button plate's buttons are the grid's `-` and
`=`, `input:0x07/0x2d@00a7` and `0x2e`), and every matching bind fires, so when two plates want different
things from one button, give each its own layer (`--layer plate:2-button`, `--layer plate:12-button`)
rather than base plus layer. `button plan --plate 12-button` shows what the firmware holds with that plate
on; `neuron watch --device` shows which plate the mouse reports.

**Learn what a button holds without pressing it.** `neuron button read --all` reads every button the
firmware lists. A keyboard key is category 0x02, a mouse click 0x01, and the wheel-tilt buttons hold 0x0e
at stock (Synapse's turbo scroll).
`neuron control capture` needs a human press and returns `no control was pressed` when nobody is there; do
not loop on it.

## Things that go wrong

- **Scope a macro key to its device.** `macro:M3` alone matches every device that sends that code, and a
  Naga side plate shares the keyboard's M-key codes, so a debug bind on it fires from the mouse too. Write
  `macro:M3@0221`. `bind check` warns about the bare form.
- **`summon` matches title or exe by substring** and cycles when several windows match. A terminal's title
  changes constantly, so name its exe (`WindowsTerminal`). `neuron.focus(title)` inside a macro is an exact
  title match and cannot do that.

- `no macro 'x'` / `no profile 'x'`: create it first, or pass `--allow-missing-refs` if the order is
  intentional. `macro rm` tells you which binds still name the macro.
- A bind on the same trigger and layer **replaces** the old one; `outcome` says `added` or `replaced`.
- `cast wedge set` past the wheel's sector count is refused: raise `--sectors` first.
- `profile apply` from the CLI writes the device from this process; `profile apply NAME --live` hands it
  to the running app's own device session, which is what the profile sheet does.
- Preferences (`config app set`) are read when the app starts; binds, cast, routes, feel timing,
  profiles, badges and macros are re-read live.
- Exit codes: 1 is a failure (including `macro run` of a missing or crashing macro), 2 is a usage error
  (unknown command, malformed argument). With `--json` both print `{"error": "..."}` on stderr.
- Indexes: `bind`, `lighting stack` and `profile route` lists count from 0; hardware stages
  (`feel stages --active`, `feel scroll STAGE`) count from 1.
- `neuron status` says `app running: yes, on another run root` when an app is up but not on your
  `NEURON_RUN_DIR`. Edits there are not signalled (`"live": "other-root"`); the app is untouched.
- `neuron list` never writes: a new device shows as ``new device NAME: run `neuron adopt` ``. With several
  devices `info` takes `--pid`.
- Device verbs are `neuron feel dpi|polling|stages|scroll|lod|brightness|game-mode|idle|sniper`; the
  top-level spellings still work but are hidden. Import a Synapse export with `neuron import FILE`.
- Engine chatter is hidden; `--verbose` (or `NEURON_DEBUG=1`) shows it.
- No running app is not an error: writes still land on disk (`"live": "no-app"`) and take effect at the
  next start. Use `--no-live` in batch scripts and a single `neuron reload` at the end.
