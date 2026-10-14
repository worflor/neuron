# Compatibility Lab

Compatibility Lab helps investigate devices I don't have on my desk. A basic [device report](https://github.com/worflor/neuron/issues/new?template=device_report.yml) is already useful; you don't need to collect anything first. If you want to provide more detail, Neuron's CLI can collect read-only HID evidence without changing your device settings.

## try it yourself

1. Find `neuron.exe` beside `neuron-app.exe`, and open PowerShell **in that folder**. Run `.\neuron.exe compat --help` to see the commands in your installed version. If Neuron is on your PATH, `neuron` works too.
2. Collect to a **new local folder**, then inspect the device names **locally**:

   ```powershell
   $folder = Join-Path $env:LOCALAPPDATA ("neuron\compat-" + (Get-Date -Format "yyyyMMdd-HHmmss"))
   $dossier = Join-Path $folder "private"
   $bundle = Join-Path $folder "shareable"
   $zip = Join-Path $folder "evidence.zip"
   .\neuron.exe compat collect --out "$dossier"
   .\neuron.exe compat inspect "$dossier" --local-names
   ```

3. Pick the device number shown in the local list. PowerShell will ask for it, then export **only that device**:

   ```powershell
   $deviceId = [int](Read-Host "Device number from the list above")
   .\neuron.exe compat inspect "$dossier" --device $deviceId
   .\neuron.exe compat export "$dossier" --device $deviceId --out "$bundle"
   .\neuron.exe compat verify "$bundle"
   .\neuron.exe compat pack "$bundle" --out "$zip"
   ```

4. Open `$bundle\summary.md` and review any drafts in `$bundle\issues\`. Write what actually happened in your own words, then [file a device report](https://github.com/worflor/neuron/issues/new?template=device_report.yml). Attach the ZIP only if you're comfortable sharing it. Neuron doesn't upload or post anything for you.

**keep the local dossier private.** It can contain serials, paths and other identifiers. The default export leaves out those fields and raw input, but you should still review it. Recording input or including raw descriptors is a separate opt-in, not part of these steps. The bundle verifies its own structure, not that your device is supported.

This is HID evidence, not automatic device support. Audio endpoints, non-HID functions and unknown output experiments need separate reviewed procedures. Missing evidence never authorizes guessed writes. [CLI reference](CLI.md#compatibility-lab) · [optional guided skill](../../skills/neuron-compat/SKILL.md).

## Ownership and architecture

`neuron-core::compat` owns schemas, capture bounds, evidence classification,
export projection and integrity checks. The CLI is a thin front-end; the
[neuron-compat skill](../../skills/neuron-compat/SKILL.md) chooses investigations
and explains them. Collection uses `transport::enumerate` and the hardware-policy
guarded `research_metadata` seam. It does not use the adoption ledger, synthesize
definitions, acquire device custody, send commands or create another dispatcher.

Windows reads zero-access HID metadata, all three report kinds and collection
links through the OS parser. PnP Container IDs supply grouping evidence; the
existing path identity is a labeled heuristic fallback. Linux reads sysfs
descriptors and USB ancestor evidence. Raw bytes are retained locally, but complete
normalized fields are not yet available there. The existing platform enumeration
can omit interfaces it cannot query; an inventory is not proof of their absence.

Multiple collections remain separate even when an OS association groups them.
Inventory is vendor-neutral; an unknown VID or missing dialect does not filter out
an enumerable HID collection. It does not imply that proprietary controls work.
Two units sharing VID/PID are not one mutable device. Group IDs are temporary
dossier references; neither driver routing nor production identity is changed by
the Lab. A container association is not evidence that a receiver and every paired
device have interchangeable command addressing.

## Evidence contract

Three outputs serve different purposes:

| Output | Contents | Intended use |
|---|---|---|
| Local dossier | OS observations, paths, product/manufacturer/driver strings, topology keys, raw descriptors and optional input recordings | Investigation on the contributor's machine; never an automatic attachment |
| Shareable bundle | Numeric descriptor facts, grouping provenance, environment/build, declared capabilities, findings, analysis and artifact hashes | Engineering triage and offline fixtures |
| Issue draft | One research lead, relevant interface IDs, missing evidence, implementation seam and acceptance criteria | Review with the contributor before publication |

The shareable bundle is built from typed fields, not copied from an arbitrary
directory or sanitized by an agent's regex. Device strings, serials, paths,
container IDs, config, logs and host identity have no field in the default export.
Raw input and raw descriptors require separate reviewed opt-ins. Numeric variation
counts are aggregate observations, not decoded controls. Descriptor payloads and
vendor input can contain private data despite this projection; the exporter does
not promise anonymity for approved raw artifacts.

Version 1 bundles contain `manifest.json`, `inventory.json`, `findings.json`,
`privacy.json`, `summary.md`, numeric `descriptors/` fixtures, aggregate
`observations/analysis.json` and relevant `issues/` drafts. Optional raw input and
descriptors live under `observations/`. The manifest names and hashes every artifact.
Verification rejects traversal, non-regular files, unsupported schemas, unexpected
files and changed hashes. Integrity does not authenticate the submitter or attest
hardware behavior. Keep edited issue text outside the bundle so its hashes remain
valid.

Verification checks findings against the inventory and aggregate analysis, and
checks that analysis against reports when raw input is included. A correctly
rehashed but contradictory bundle is rejected. Without raw reports, aggregate
counts can be checked for bounds and references, not independently reproduced.

`inspect --local-names` exposes product names only for local target selection.
`inspect --device ID` and `export --device ID` retain a selected group and all its
collections/trials without renumbering them. The source dossier stays complete.
Names remain untrusted device data; an OS group can include several related
functions rather than a single independently addressable unit.

Native `pack` produces one ZIP attachment using only verified artifacts, with no
local-dossier traversal. `verify` accepts a directory or ZIP. Archive input and
expanded contents are bounded to 64 MiB, individual entries to 16 MiB and total
entries to 8,193. Traversal, symlinks and duplicate entries are rejected before
temporary extraction. Archives and collected bytes remain local until explicitly
approved for publication.

Support evidence remains separated: descriptor observation, protocol signature,
registry declaration, a protocol response, a device-write read-back and physical
confirmation are distinct facts. Inventory attests only the first three. It does
not promote a declared setter into a verified feature or turn missing data into
unsupported hardware.

## Passive input recipe

`vendor-input-v1` records one selected interface after explicit consent. It
revalidates the interface and descriptor before opening the existing `InputReader`.
Only complete vendor-input metadata without declared keyboard fields is eligible;
standard keyboard/mouse collections are refused. Duration is at most 30 seconds,
report count at most 2,048, report size at most 4,096 bytes and captured payload at
most 256 KiB. Read failures retain
the observed prefix with an explicit end reason. No dispatch or action synthesis
occurs.

The contributor presses/releases one named control three times. The analyzer
compares consecutive reports separately by report ID and size and counts rising
and falling bits. It does not label a button automatically: counters, checksums,
battery notifications and unrelated activity can also change. Trial intent comes
from the contributor. A mapping requires isolated repeat evidence and a decoder
regression. The recipe stops on refusal or access failure rather than elevating
or installing interception.

## Investigation without the hardware

`neuron-testkit::compat::RecordedInput` consumes the core recording schema and
implements `InputReader`. A test can feed the exact report order into a production
decoder, preserving short edges and rejecting truncation. Timestamps are retained
as evidence; playback is driven by the test without real-time sleeps.

`RecordedProtocol` implements `Transport` over a reviewed `ProtocolTape`. Exact
request bytes and channels must match; ordered replies retain their real lengths
and may represent timeout or disconnect. It has no hardware fallback. These tapes
are an offline fixture format, not collected by the public inventory command.
Approved capture procedures must establish their provenance before they become
wire facts or committed fixtures.

The CLI's `replay` reproduces neutral bit analysis only. A passing replay is not a
passing device decoder, driver, hardware write or physical-output test.

## Extending the Lab

Additional recipes should use this service rather than parallel hardware control:

1. Give the recipe a versioned ID, a narrow selected target, explicit required
   evidence and bounded resource/time limits.
2. State whether it reads OS metadata, observes input, sends recognized getters
   or changes device state. An output report's declared size is never its semantics.
3. Route through existing transport locks, ownership, protocol and safety gates.
   Interception and device-write changes require owner approval under `AGENTS.md`.
4. Keep typed outcomes separate from errors and physical observations. Missing or
   timed-out evidence must remain unknown.
5. Define local retention, an export allowlist and offline fixtures before exposing
   the recipe to agents. Do not publish automatically.

The highest-value follow-ups are reviewed recognized-getter transcripts, complete
Linux field normalization, declared-versus-observed input-source comparisons and
an inline SYSTEM export entry point. Each needs its own verification. Unknown
firmware fuzzing, guessed setters and automatic driver installation are outside
the contributor workflow.

Platform references: [Windows Container IDs](https://learn.microsoft.com/en-us/windows-hardware/drivers/install/devpkey-device-containerid),
[Windows HID capabilities](https://learn.microsoft.com/en-us/windows-hardware/drivers/ddi/hidpi/ns-hidpi-_hidp_caps),
[Windows interface metadata](https://learn.microsoft.com/en-us/windows/win32/api/setupapi/nf-setupapi-setupdigetdeviceinterfacedetailw),
and [Linux HID descriptors](https://kernel.org/doc/html/latest/hid/hidintro.html).
