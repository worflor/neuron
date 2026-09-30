# neuron CLI

Everything the app can author, the CLI can author: binds, rebinds, macros, the cast wheel, glyphs,
lighting, profiles, routes, badges, feel, preferences, and the whole setup as one document. Reads are
machine-readable, writes are validated before they touch disk and echo the state that landed, and a
running app picks the edit up without a restart.

Grammar: `neuron <noun> <verb> [args]`. `neuron --help` groups the commands (Devices, Feel, Input, Lighting,
Setup); `neuron <noun> --help` lists the verbs, `neuron catalog --json` lists everything that can be named.
The device verbs live under `neuron feel`; the older top-level spellings (`dpi`, `polling`, `dpi-stages`,
`scroll`, `lod`, `brightness`, `game-mode`, `sniper`, `idle`) and `import-export` still work but are hidden from
help, as are the debug harnesses (`prof`, `probe`, `discover`) and `bind legacy|init`.

## Conventions

| | |
|---|---|
| Output | `--json` on any verb prints one JSON document on stdout. Reads emit their data; writes emit the resulting state (re-read from disk) plus a `live` field. |
| Errors | one plain line `error: ...`, or with `--json` `{"error": "..."}`, on stderr. Exit code 1 for a failure (bad value, missing thing, failed run), 2 for a usage error (unknown command, missing or malformed argument). Nothing is written on a rejected command. |
| Reads | reads never write. `list` reports a device no definition covers as ``new device NAME: run `neuron adopt` to learn it``; only `adopt` writes `devices/auto/`. With several devices, `info` describes the first and says `(1 of 3 devices; --pid to pick)`. |
| Chatter | engine start-up lines (`[macro host] ...`) are hidden; `--verbose` or `NEURON_DEBUG=1` shows them. |
| Run root | every config file lives under one directory (`neuron config path`). `NEURON_RUN_DIR` points a session at a scratch directory; tests and dry rehearsals should use it. `neuron status` says whether an app runs and whether it uses this root (`yes, on another run root`). |
| Live reload | a config write signals the running app, which re-reads within about a second (`live`: `reloaded`, `no-app`, `other-root`, `skipped`, `failed`). The signal only goes to an app that uses the same run root: an app publishes its root in `app.pid`, and when `NEURON_RUN_DIR` is set and no such file matches, nothing is sent (`other-root`). `--no-live` skips the signal; `neuron reload` sends it once. Preferences (`config app`) take effect when the app next starts. |
| Indexes | lists (`bind`, `light stack`, `profile route`) count from **0** and are printed by `list`. Edits address an index; `bind rm` also takes `--trigger`. Hardware stages count from **1**, as Synapse numbers them: `dpi-stages --active`, `scroll STAGE`. |
| Specs | `@file` reads a spec from a file, `-` from stdin. |
| Safety | no verb here synthesizes input. Device writes (`button apply`, `idle`, ...) re-read the device and fail loudly on a mismatch; `button apply` also needs `--arm`. The gated writes stay gated. |

### Triggers and actions

A trigger or action is written as a shorthand, an inline JSON object, or an inline TOML table; all three
are the same typed value the config files hold.

```text
--trigger mouse:4                     --action key:ctrl+shift+s
--trigger key:f13@00a8                --action dpi:1600
--trigger macro:M1                    --action macro:my_macro
--trigger input:0x09/0x04@00a8        --action run:notepad.exe
--trigger 'label:Left Ctrl'           --action keys:w:180 ~90 a
--trigger gesture:circle              --action '{"type":"dpi-set","dpi":800}'
--trigger radial:3                    --action '{type="key", key="F5"}'
--trigger app:valorant
--trigger mic-tap  hold:LAYER  cast:2  game-light:APP/EFFECT
--trigger '{"kind":"input","page":9,"usage":4}'
```

`neuron action list` prints the shorthand ids with a short description of what each does, its parameter
grammar, and a JSON example of every action variant (`does` in `--json`); `neuron trigger list` does the same for triggers. `neuron action check SPEC` and
`neuron trigger check SPEC` resolve a spec, print it normalized as JSON and TOML, and report problems
without writing.

Validation is the GUI's own (unknown keys, DPI range, cps 1-50, empty sequences, scene names, ...).
An unknown key gets a suggestion (`'ctrlx' isn't a key. Did you mean ctrl+x?`). A `turbo:f` with no rate runs
at 10 cps and the confirmation says `(10 cps is the default ...)`; `turbo:f · 12` sets it.
A macro or profile that does not exist yet is an error unless `--allow-missing-refs`, which downgrades it
to a warning in the reply.

## Reference

### bind: the Trigger -> Action spine

| verb | |
|---|---|
| `bind list [--layer L \| --base] [--profile P]` | binds with their indexes |
| `bind show INDEX` | one bind |
| `bind add --trigger T --action A [--layer L \| --hypershift] [--profile P] [--allow-missing-refs]` | add; the same trigger on the same layer is replaced. `--capture` presses a control instead of naming one |
| `bind set INDEX [--trigger T] [--action A] [--layer L \| --base]` | edit in place |
| `bind rm INDEX` / `bind rm --trigger T [--layer L]` | remove |
| `bind mv FROM TO` | reorder |
| `bind clear (--base \| --layer L) --yes` | remove a whole layer |
| `bind hold show \| set --trigger T \| clear` | the HyperShift hold key |
| `bind check --trigger T --action A` | validate only |
| `bind live` | the spine as the engine assembles it (binds, cast wedges, glyphs, rhythms, profile binds) |

Without `--profile` binds go to `profiles/gui.rules.toml`, live whatever profile is active. With
`--profile P` they go to that profile's own file and are live while it is active. The HyperShift layer
stance (hold, latch, smart, one-shot) is `feel timing --hypershift`.

### control, action, trigger, catalog

`control list [--pid P]` devices with their controls and bindable specs. `control catalog [--page
keyboard|mouse|consumer|macro|all]` every named control. `control name PAGE USAGE`. `control capture
[--seconds N] [--pid P]` press a control, print its trigger (Windows; with the app running and firmware
functions applied, a pressed button can report its remapped key: name it with `label:` or `input:`).
`catalog` prints actions, triggers, lighting presets and knobs, emblems, profile fields, preference keys
and setup sections in one document.

### button: firmware button functions (rebinding)

A bind whose trigger is a firmware-assignable button is performed by the device itself when the action is
a single key or chord; anything else is performed by the app's interceptor on a private F13-F24 key. The
app applies this plan whenever input is armed.

| verb | |
|---|---|
| `button plan [--pid P] [--plate LABEL]` | per button: `stock`, `firmware` or `host`, and the 7-byte record (no hardware touched). `--plate 12-button` plans for that seated side plate; without it every plate's binds count |
| `button read [--pid P] [--all] [--profile N] [--hypershift]` | what each button holds now (read-only). `--all` walks every button the firmware lists (`02/84`), not just the thumb grid: clicks, tilt, plates. `--profile` 0 is the live direct profile, 1..5 the onboard slots |
| `button apply [--pid P] [--plate LABEL] --arm` | write the firmware part, each write read back; host-performed binds stay stock (the CLI has no interceptor). Volatile: the device forgets on replug |
| `button restore [--pid P]` | factory functions, verified |

`neuron remap` is retired: it wrote `15/02`, which never changed what a key emits. It now authors the bind
(`--key <stock key> --to <key>`), and `--reset` points at `button restore`.

### macro

`macro list`, `macro show NAME`, `macro add NAME (FILE \| - \| --source TEXT) [--mode raw\|bound]
[--no-check]`, `macro rm NAME` (reports binds still naming it), `macro mode NAME raw\|bound`, `macro check
(FILE \| - \| --name N)`, `macro run (NAME \| --file F) [--arm]` (input helpers are traced, not fired,
unless `--arm`; `macro run --file F` is the GUI's test button, a candidate run once and never stored), `macro options NAME [--set JSON]`, `macro prelude`. A macro runs from a trigger with
`--action macro:NAME`. `add` checks the source with the bundled Python and registers it warm;
`--no-check` writes the file without a runtime.

### cast, gesture

`cast show`; `cast set [--trigger T \| --capture] [--activation "tap hold"] [--sectors N] [--deadzone D]
[--assist 0-0.6] [--mode auto\|radial\|gesture] [--hyper on\|off]`; `cast wedge list|set N --action A
[--hyper]|clear N [--hyper]`; `cast rhythm list|set TAPS --action A|rm TAPS`; `cast glyph bind NAME
--action A | unbind NAME`; `cast init`, `cast run`.

`gesture list`, `record NAME [--trigger T]`, `rename OLD NEW`, `delete NAME`, `clear --yes`, `bind NAME
--action A`, `unbind NAME`, `tune ...`, `match`, `selftest`. Recording needs the physical stroke; binding,
renaming and deleting do not.

Limits are the GUI's: 3 wedges up to what the deadzone allows, assist 0-0.6, activation phrases parse as
`tap`/`hold` (a hold only last).

### lighting (`light` is an alias)

A look is a bottom-up stack of layers: a pattern, its knobs, a colour spectrum, an optional region, a
blend. It lives on a profile (`--profile P`, painted by `profile apply`) or as a device's saved look
(`--pid HEX`, resumed by the app at launch).

| verb | |
|---|---|
| `lighting catalog` | presets, patterns, knob schemas (range, enum options, toggles) |
| `lighting stack list --profile P` | layers, bottom first, with problems |
| `lighting stack add (--preset S \| --pattern K \| --spec JSON) [--color RRGGBB] [--gradient A,B,..] [--motion drift:0.5] [--blend add] [--param k=v]... [--region 0,1,2 \| --rect r0,c0,r1,c1 --board 6x22] [--disable] [--at N]` | add a layer |
| `lighting stack set INDEX ...` | edit in place |
| `lighting stack rm INDEX`, `mv FROM TO`, `clear --yes`, `replace JSON` | |
| `lighting fps --pid HEX [N]` | stream frame rate 1-30, `0` clears |
| `lighting apply NAME` | paint a profile now (same as `profile apply`) |

Knobs are checked against the pattern's own schema; an enum knob takes an option name or index. A
hand-painted per-key frame is a `custom` layer: `--spec '{"pattern":"custom","frame":[[r,g,b],...]}'`.
The older `lighting run|effect|mirror|keytest|cellsweep|cells` verbs are the hardware bench and unchanged.

### profile

A profile is settings, a lighting stack and its own binds.

`profile list|show NAME|active`; `profile new NAME`; `profile set NAME KEY=VALUE...` (fields: dpi,
dpi-stages, polling, brightness, idle-secs, in-game-polling, disable-alt-tab, disable-win, disable-alt-f4,
disable-alt-esc, persist; `unset` clears an optional one); `profile save NAME --dpi ...`; `profile apply
NAME [--live]` (`--live` has the running app apply it through its own device session); `profile rename
FROM TO` (binds and routes follow); `profile delete NAME --yes`; `profile capture NAME` (reads the
hardware); `profile export NAME [--out F]`; `profile import FILE [--replace]`.

`profile route list|add APP PROFILE|rm INDEX|mv FROM TO|default PROFILE|--clear`: which focused app
switches to which profile. First match wins; `default` is where a non-matching app returns.

### feel

`feel show`; `feel timing [--hold-ms N] [--gap-ms N] [--coyote-ms N] [--hypershift hold|latch|smart|one-shot]`;
`feel sniper [--trigger SPEC \| --trigger] [--dpi N] [--unbind]` (`--bind` is the old spelling; bare `--trigger` presses a control); and the device verbs
`feel dpi|polling|stages|scroll|lod|brightness|game-mode|idle`, identical to their top-level forms. Every
device write is verified by read-back.

### badge

`badge list`, `badge set PID [--emblem mouse|keyboard|keypad|pad|stick|headset|mic|dial|device] [--name N]`,
`badge clear PID`.

### config, status, reload

`config path`; `config app list|get KEY|set KEY VALUE|unset KEY` for the GUI's `app.toml` (notifications,
accents, phoenix, start_minimized, the CONNECTIONS settings, ...; values are type- and range-checked;
secrets such as the OBS password are never read or written here). `status` gives version, run root,
whether the app runs, the active profile and counts. `reload` signals the app.

### dump, apply: the whole setup

`neuron dump [--only rules,cast,...] [--vault] [--out F]` writes TOML (JSON with `--json`). Sections:
`rules` (always-live binds), `cast`, `feel`, `apps` (routes), `bindings`, `profiles` (each with lighting
and binds), `badges`, `macros` (sources) with their option values, `app` (preferences, secrets removed),
and with `--vault` the glyph shapes.

`neuron apply FILE [--dry-run] [--prune] [--strict] [--check-macros]` makes the machine match the
document. A section in the document is authoritative for that part; a section left out is untouched.
Everything is validated first and nothing is written if anything is wrong. Applying is idempotent: a
section already equal to disk is reported `unchanged` and not rewritten. `--prune` also deletes profiles,
macros, badges and preference keys the document omits; `--strict` makes unresolved macro/profile names
errors. The reply lists each section with `unchanged`, `updated` or `would-update`.

## JSON shapes

```text
bind list/add/set/rm/mv   {store, path, rules:[{index, layer, trigger:{kind,...}, action:{type,...},
                           trigger_text, action_text}], outcome?, index?, rule?, warnings?, live}
action check              {ok, action, action_text, toml, issues:[{severity, message}]}
cast show                 {trigger, activation, sectors, deadzone, assist, mode, hyper_radial_on,
                           wedges:[{index, compass, action}], hyper_wedges, glyphs, rhythms, slots, complaints}
profile show              {name, summary, profile:{...}, binds:[rule...]}
lighting stack *          {target, layers:[LayerDef...], issues, paint, live}
control list              {devices:[{pid, name, emblem, def, controls:[{name, spec, trigger, ...}]}]}
dump / apply              a setup document / {sections:[{section, status, detail}], issues, applied, live}
```

## Appendix: capability matrix (GUI -> CLI)

| GUI | CLI |
|---|---|
| Bindings: add (press-to-bind), edit, remove, reorder, base and HyperShift, hold key, stance | `bind add --capture`, `set`, `rm`, `mv`, `hold`, `feel timing --hypershift` |
| Every action in the picker, and every `Action` variant | `--action` shorthand or JSON/TOML; `action list`, `action check` |
| Triggers: control, glyph, radial sector, app focus, mic tap, hold layer, cast rhythm, game light | `--trigger` of each kind; `trigger list` |
| Controls with names, capture a control | `control list`, `control catalog`, `control capture` |
| Rebinding a device button in firmware | a bind, then `button plan`, `button apply --arm`, `button restore`; the app applies it itself |
| Macros: list, open, save, delete, options, raw/bound | `macro list|show|add|rm|options|mode|check|run` |
| Macro block editor | write the Python source (`macro add`); `macro prelude` documents the API |
| Cast: trigger, rhythm, sectors, assist, hypershift wheel, wedges, rhythm map | `cast set`, `cast wedge`, `cast rhythm` |
| Glyphs: record, rename, delete, clear, bind | `gesture record|rename|delete|clear|bind`, `cast glyph` |
| Lighting: effect tiles, stack, colours, knobs, motion, region, blend, fps | `lighting catalog`, `lighting stack`, `lighting fps` |
| Lighting: hand-painted frame, import a light file | `--spec` custom layer; `import FILE` |
| Profiles: capture/save, apply, rename, delete, default, app rules | `profile new|set|save|capture|apply|rename|delete|route` |
| Device badges | `badge list|set|clear` |
| Feel: DPI, stages, polling, brightness, scroll stage, LOD, sleep timer, game mode, sniper | `feel ...` (and the top-level verbs) |
| Timing windows, tap/hold, coyote | `feel timing` |
| Settings: accents, notifications, connections, phoenix, launch to tray | `config app ...` |
| Import wizard | `import` (survey, lists Synapse 3 mapping logs), `import FILE [--apply]` (a Synapse export, or a Synapse 3 `*Mapping*.log`) |
| Onboard storage, backup, verify | `storage`, `backup`, `verify` |
| Pockets, audio endpoints | `pocket`, `audio` |
| Everything authored, at once | `dump`, `apply` |

Not reachable from the CLI, and why:

| GUI | why |
|---|---|
| Scroll stage table, in-game polling split, Snap Tap | the writes are behind `hyperscroll-write`, `ingame-poll-write`, `snap-tap-write` until confirmed on hardware; the CLI does not enable them. Profile fields for them can still be authored |
| Live lighting preview, brush painting, stroke previews | interactive by nature (`cast run`, `radial pick`, `lighting run` are the terminal equivalents) |
| Teleport/whiteboard "try now", overlay instruments | drawn by the resident app; bind them (`--action teleport`) |
| Chroma lab names, hides, scenes | held only in the app's lab state; binds to lab effects work (`--trigger game-light:APP/EFFECT`) |
| Weave material knobs, input arm stance, pause writes | per-session app state, not persisted |
| OBS password | a secret; set it in the app |
| Start with Windows | a registry value owned by the app |
| Purge Synapse | destructive and confirmed in the app |
