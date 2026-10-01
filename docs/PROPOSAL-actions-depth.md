# Action depth — implementation brief

> **status:** refined implementation direction. Not a claim of shipped features.
> One Trigger -> Action pipeline; GUI, CLI, radial, glyphs and sequences share typed actions.

## Clipboard transform

Transform Unicode clipboard text without typing into the focused app. Pure operations
serve preview, validation and execution. One ordered list of typed operations:

```toml
{ type = "clipboard-transform", ops = [{ op = "trim" }, { op = "lines-unique" }] }
{ type = "clipboard-transform", ops = [{ op = "regex", pattern = '(?P<date>\d{4}-\d{2}-\d{2}) (?P<msg>.*)', replace = '[${date}] ${msg}' }] }
```

| family | operations | contract |
|---|---|---|
| case | uppercase, lowercase, titlecase | Unicode-aware simple word casing, not locale-sensitive editorial casing |
| whitespace | trim, trim-lines | whole text or each line |
| lines | lines-reverse, lines-sort, lines-unique | stable dedupe; preserve terminal newline and CRLF style |
| structured | json-pretty, json-compact, csv-to-json, json-to-csv | invalid input fails; CSV has headers and quoted fields; JSON-to-CSV requires rectangular object rows with scalar values |
| encoding | base64-encode, base64-decode, url-encode, url-decode | UTF-8; malformed encoding/non-text fails; URL encoding handles one component |
| substitution | regex | Rust regex, $1 and ${name} replacement tokens; unknown captures fail |

Bound input/output to 1 MiB, operations to 32, regex compilation and preview samples.
Validate before live access; compute the whole result before writing. If the clipboard
sequence changed meanwhile, refuse to overwrite a new copy. Distinguish busy, empty,
unsupported format, invalid text and operation errors. Arm-gate mutations. Never clear
rich contents until replacement is ready; the result is explicitly plain text. Unchanged
output is a no-op.

### Example editor

Inline beside the shared action picker: sample, pattern/replacement, result. Opening
never reads the actual clipboard automatically. Construction is deterministic: choose
variable sample text, a capture name and kind (literal, digits, word, text until the next
delimiter). Escape fixed text and show the real pattern. Never infer generality from one
example. Named replacement buttons insert ${name}. Distinguish match, no-match and invalid
pattern. A raw-pattern mode shares the preview. No WASM, service, AI inference, modal or
new page. Reopening a complex action must preserve its spec. General chains use the
existing structured-spec escape hatch rather than a new visual language.

## Lighting layer

Manipulate the current host compositor stack through existing validation, persistence
and reload. No new HID opcode, writer or firmware effect. Address indexes: **0 is bottom**,
matching the CLI. Preset names are not unique layer identities; no parallel name registry.

```toml
{ type = "lighting-layer", op = "toggle", index = 1 }
{ type = "lighting-layer", op = "push", preset = "fire" }
{ type = "lighting-layer", op = "move", from = 2, to = 0 } # to bottom
{ type = "lighting-layer", op = "params", index = 0, params = { speed = 1.5 } }
```

Operations: toggle, enable, disable, push, pop, replace, move, spectrum, params, region.
Push/replace use a preset or typed LayerDef. Spectrum uses existing Spectrum serialization.
Params validate schema keys/ranges; empty region clears its mask. Empty pop, bad index and
invalid knobs fail. Move inserts at its final index after removal. Clone/edit/validate
before atomic persistence; failure leaves saved and live state unchanged.

The app targets its selected stack on the UI thread; the daemon uses the known active
profile/target or reports unavailable. Never guess a HID handle. Respect write pause
when an edit would cause live output; disarmed actions cannot mutate. Clamp selected
layer, preserve unrelated layers and use the existing stream. Repeated push stacks deliberately.

## Dial scroll

Extend DialTarget with hover and anchored vertical scroll. Preserve audio compatibility;
new targets round-trip without silently falling back to volume. Hover scrolls at the
current pointer. Anchored captures pointer/window at each hold start, for that hold only.
A fresh hold recaptures. Losing the target or changing desktop ends scrolling safely.

Use the existing eigenmotion dial and weave lifecycle. Accumulate fractional wheel units,
bound emissions and clear remainder on release. Priming never scrolls. Anchored delivery
must target the saved window/pane without moving the physical cursor: SendInput alone
cannot do that. Handle nested panes and negative coordinates. Arm-gate input and report
unsupported platforms. Overlay shows scroll rather than percentage; target stays in the picker.

## Pocket history and management

Reuse full-fidelity Pocket snapshots/store. CLI list, inspect, delete and clear-history;
an inline forget control on the existing cards. Delete durable files before claiming
success, bump the existing generation counter, validate slot names and contain paths.

History is **neuron's session history of explicit clipboard operations**, not Win+V or
an invisible global clipboard recorder. Successful actions remember displaced carryable
contents. Bound to 20 entries and 16 MiB total, dedupe consecutive snapshots, report
oversized/unsupported snapshots honestly. No default persistence. List summaries/sizes;
bodies require inspect. PocketHistory { index } restores newest-first without Ctrl+V,
consuming/reordering entries or recursively feeding history. Missing/busy/disarmed fails.
Named pockets keep their stash/swap semantics.

## Run: make capability legible

Label **open / run**, with concrete app, URL and shell examples. Inspect actual semantics:
a bare URL is not a Windows command; use the existing shell's quoting:

```toml
{ type = "run", cmd = "code" }
{ type = "run", cmd = 'start "" "https://example.com"' } # Windows
```

Run and ScriptKind::Shell share the shell helper today. Only Python macros use the warm
Macro Host; shell scripts do not gain Python context or queuing. Keep compatibility
variants, quoting and process-spawn gates. No Python bridge changes for wording.

Keep MicGainSet (absolute percentage) and MicGain (relative percentage) as distinct
operations; their similar names do not justify losing either contract.

## Undo: restore state, never guess

One session journal of 20 successful reversible mutations: concrete resource, previous
value and successfully applied value. Audio mute/gain, explicit profile switches and
lighting stack edits where their existing owner can restore them. Audio records endpoint
**IDs**, not a later default or substring. Honour bounds and setter failures.

Undo restores the newest reversible change through the same runtime seam. Compare current
state to the applied value first and refuse stale restoration. Pop only after success;
never journal undo. Empty/stale/disarmed/unsupported results are explicit. No entries for
failure, no-op, dry run or unknown backend. Keys, clicks, pasted text, launches, kills, OBS,
screenshots and scripts have no inverse. Never replay a key and call it undo. Echo stays
separate. Nested actions do not duplicate entries: undo means the last reversible change,
not an arbitrary whole sequence. Keep history across profile switches so those can be
undone; clear at session teardown. Tests use fake state and never arm input.

## Screenshot

One action, screen/window/region modes, clipboard default, optional file path:

```toml
{ type = "screenshot", mode = "screen", clipboard = true }
{ type = "screenshot", mode = "window", clipboard = false, path = "shot.png" }
{ type = "screenshot", mode = "region", clipboard = true }
```

Screen means virtual desktop including negative coordinates. Window means visible pixels
in the trigger-time focused window bounds; no promise about occluded backing pixels.
Region uses a native drag overlay; Escape cancels without changes. Hide selection ink
before capture. Retain target before overlay/toast focus changes. Validate geometry,
allocations and destinations, bound pixels, check GDI returns and release every handle.
Save PNG with collision-safe default names under the run root. Clipboard is a proper
Pocket-compatible DIB. Only explicit armed actions capture. Test geometry/encoders with
synthetic pixels. Report platform, clipboard and file errors honestly. No capture manager.

## Input page

Keep wire rows, badges, base/HyperShift bar and inline editor. Search above the list filters
control/device/action text. Show a bounded slice (eight rows starting measure), matching
count and previous/next controls. Add-bind remains nearby. No nested scroll pane, card
wall, modal or new page. Small lists do not need pagination chrome.

Retain **source indexes** through filtering/paging; edit/delete/reorder use original indexes.
Search never changes dispatch precedence. Query/layer changes reset paging, mutations
clamp it and edited rows remain visible. Disable reorder when filtering hides its
consequence. Do not replace live models on timer ticks.

## Integration and verification

Every action belongs in enum, description, catalog, validation, palette round-trip, setup
import/export, CLI and app/daemon runtime. Test bad inputs and fake successful transactions.
Semantics live in core; GUI projects them. Tests stay disarmed and deny hardware transport.
Run validate.ps1 at milestones, full mode before final integration. Launch the Windows app
and inspect changed surfaces. Record actual hardware evidence separately from builds.
Update README/GDD/CLI/STATUS honestly. No push, release or co-author trailers.
