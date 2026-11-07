# Changelog

## Unreleased

- Worker heartbeat ages now exclude time the PC spends asleep, and a gap of more than five seconds between heartbeat checks is treated as the whole app having been frozen rather than as each worker stalling. Waking from sleep or Modern Standby should no longer raise an "internal stall" warning or write a stall dump to the crash log. Verified by unit tests while awake; the resume path itself has not been exercised in an automated run.
- BOUND macros (the default mode) can now use the OBS helpers and `neuron.signal`. They were missing from the module's export list, which BOUND uses as its capability list, so calls failed with "bound neuron has no capability". Effect helpers now return the same shapes in both modes: `True` or the host's confirmation when the effect happened, and a falsy `"[reason]"` string when it did not. Host-side failures such as a failed brightness write used to come back as plain text that looked like success, and a BOUND `focus` on a missing window reported success. Verified by an end-to-end test of both modes with input disarmed, and on the owner's desk with input armed: a BOUND macro on a keyboard M-key typed its key, `key` returned `True`, an out-of-range `signal` returned its bracketed reason and `obs_connected` returned `False` with OBS closed.
- A stall dump now records each watched worker thread's scheduler state, wait reason and CPU time, then the CPU each used over the next second, so a stall reads as stuck, busy or starved. Most stall dumps in the owner's log happened while the PC was awake; their cause is not yet known.
- Generated device files no longer claim that a round-trip was measured when none was, and say that their setters were paired with an answering getter but never sent. A file for a dialect that probed nothing no longer claims any command answered.
- The device report form asks whether Windows Dynamic Lighting is on and accepts Neuron's generated device file.
- Added a source-derived BlackWidow V4 X descriptor fixture from two identical public upstream dumps. It pins one control pipe, the M-key report collections and a Windows Dynamic Lighting LampArray. No V4 X support is claimed.
- The full validation gate now fails `neuron-core` lint on unchecked `Instant - Duration` subtraction, the pattern behind an early-session panic fixed in September; the other crates already had this lint.
- Generated definitions for devices without DPI on the extended (0x0F) lighting protocol, such as the BlackWidow V3 and V4 X keyboards, now target the whole board instead of the logo region. Effects and custom-frame rows carry the bytes Razer's own Synapse modules send those boards, and spectrum and wave ids follow the same source. Brightness is written and read back on the backlight region, which Synapse reads and openrazer writes. The custom-frame row layout matches a public capture of Synapse driving a V3. Mice keep their existing values. Unverified on a V3 or V4 X; no support is claimed.
- A generated definition from earlier versions with the old logo-region bytes on the keyboard grid is no longer loaded. The next adoption pass moves it to `.toml.v1.bak`, rebuilds it, and reports the move. A file whose lighting was edited by hand, or whose backup already exists, is kept and loads as before; other edits stay in the backup. Generated files now carry a `synth-version` line.
- Quitting from the tray stops live dispatch first, with a two-second limit on waiting for its worker, and releases the tray's hold on runtime state before exit. On Windows, a graceful quit previously panicked in process teardown when the runtime joined an already-terminated thread. A tray quit of the release build on the owner's desk exited without a panic or crash-log entry.

## v0.1.6

- Preserved lighting ownership across host recovery and fixed OpenRGB single-LED updates after surface resizing.
- Refreshed macro-key eligibility after keyboard adoption, with held-key state shared correctly across sibling collections and aliases.
- Rejected malformed Razer discovery replies and incomplete HID++ replies, while preserving Windows' padded short-report handling. Added source-derived offline protocol, adoption and reconnect regressions; no new hardware support is claimed.
- Added offline Windows copy inventory (`neuron-update.ps1 -Action status`, with `-Json` for agents), separating the running repo build, installed release checkpoint, startup target and config paths. Version checks read passive package metadata; unknown source-build versions remain unknown. Ambiguous startup targets can be inspected without selecting an update destination.
- Repository builds preserve another copy's Windows startup target; `release.ps1 -UseAtStartup` makes reassignment explicit. New checkpoint installers preserve other running/startup copies, and checkpoint uninstall retains their startup target and shared Chroma broker. User configuration stays separate from payload removal. Real setup/UAC/uninstall transitions still need a clean Windows VM run.
- Already-current updates avoid redundant downloads and backups, updater scratch directories are cleaned on reported outcomes, and local packaging requires `-ReplaceExisting` to overwrite existing versioned assets. Local packages retain version-specific checksum files. Windows lifecycle fixtures run in the normal validation gate; app tests keep line-table debug information to reduce build memory.
- Chroma broker updates verify a protected replacement before stopping the working copy. Failed replacements restore the previous files and task; failed rollback retains a protected recovery copy for inspection. Uninstall preserves unresolved recovery state, and interrupted first installs can resume from their verified ownership record.

## v0.1.5

- Added Compatibility Lab: vendor-neutral HID inventory, Windows descriptor fields and container grouping, device-scoped sanitized evidence bundles and issue drafts. Bounded vendor-input trials require explicit consent; offline input and protocol replay support investigation without the original hardware. Inventory does not probe protocols, adopt devices or enable dispatch. Linux descriptor collection is implemented for source builds and remains hardware-unverified; incomplete Linux field metadata currently prevents vendor-input trials.
- Effect names in the lighting inspector now use the applied palette, with a hover colour drift and a plain-text fallback for effects that bypass the spectrum. Long names are bounded and stale palette colours are cleared when switching effects.
- Ripple retains active waves during rapid typing and expires completed waves before accepting new strikes.
- Made the lighting editor more responsive: pattern simulation, thumbnail rendering and saved-effect file operations run off the UI thread; offscreen catalogue tiles sleep, and live models retain their identity during updates.
- Tightened the lighting inspector with a colour-mix dropdown, a colour-motion picker with reversible direction, compact timeline actions and inline effect property pickers. Existing authored motion and gradient interpolation remain intact.
- Lighting layers now have an ordered, scrollable stack with visibility, duplication, per-layer opacity and a blend dropdown (Normal, Screen, Add, Multiply, Cutout). A covered-layer shortcut points to an opaque effect hiding the selection. Existing scenes retain full opacity by default.
- Preserved running layer state during palette edits and stack rearrangement, and added dissolves for authored scene changes through the same bounded transition in local and hosted lighting. Continuous edits publish immediately; the live preview observes generated base frames. Explicitly clearing the final layer saves a managed black scene. The owner reports live lighting fades working in regular use.
- Added a hover-only layer insertion capsule with above/below choices. Centred its labels and improved palette names: stock ramps retain their names, recognizable altered ramps show an edited suffix, and unrelated palettes remain custom.
- Added gradient presets to the spectrum editor: rainbow, fire, aurora, thermal, sunset, meadow, ice and bubble (a soap-film ramp computed from thin-film interference). One click swaps a layer's stops and keeps its motion and blend; the editor header names the current palette in its own colours.
- Added suggested pairings on effect tiles. A small chip opens in place to apply an effect with another gradient or mode, such as Comet with sunset and light painting, or Cascade in matrix mode with bubble. Hand-picked pairings come first and the rest are scored from the gradient's lightness, hue spread and contrast; an effect is never offered its own gradient.
- Gave Typing Heat and Fire a dark foot on bright gradients so heat still reads dim to bright in the gradient's own hue.
- Lengthened Comet's light-painting fade to about 30 s (was 2.6 s) so repeated passes stack.
- Switched palette motion to high-precision timing, and added an offline key script so input-driven patterns can be rendered without synthesizing input.
- Corrected same-length effect replacement and held-key Comet retriggering. Centralized palette boundary sampling, removed arbitrary animation-clock wraps, fixed repeated-timestamp Heat advancement, and smoothed the breathing cycle join.
- Refined material responses: event colours can stay attached to their particles, heat extinguishes cleanly, and carried emission resolves in linear light without changing existing layer blend semantics.
- Added a render smoke test that draws the lighting page, hovers a pairing chip and fails on Slint binding loops.
- Refreshed the front-page documentation and effect clips, and added reproducible offline effect and promo rendering tools. These tools are separate from live device input.

## v0.1.4

- Made binding and lighting UI more intentional: grouped action selection, inline controls, compact filtered bind lists, and device identity in the lighting preview.
- Added general controller input on Windows, stick-driven radial casting, verified Xbox-protocol pad haptics, device badges and sensor reads. Switch Pro startup remains gated; Tartarus V2 is untested.
- Added Wildlife, a seasonal cellular lighting habitat shaped by typing, with idle recovery and region-aware dynamics. Refined Fire wind, Starlight twinkles, Reactive and Ripple contact response, Typing Heat, and saved-effect editing.
- Added clipboard transforms and session history, screenshots, lighting-layer actions and session Undo; expanded Pockets and dial scrolling.
- Expanded the CLI to everything the app can author: `neuron <noun> <verb>` commands with `--json` output, validated writes that echo the saved state, live reload of a running app, and the whole setup as one document.
- Hardened controller identity, firmware-backed Naga binds, side-plate handling, clipboard formats, screenshot reporting, nested actions and editor state through adversarial testing. Added Synapse 3 mapping-log import.

## v0.1.3

- Added a Chroma lab under LIGHTING → VISITORS that splits a game's lighting into ambient, held keys, animated keys and recurring effects, with per-game ids, names, replay, hiding and a colour lens.
- Added game scenes with optional notifications when a flagged scene ends while the game is in the background, and feedback-only game-light rules (clip, OBS scene, profile, mute).
- Added a Game Light layer that carries a Chroma game's colours to any device.
- Rewrote native Chroma decoding against each device type's record layout; non-keyboard devices decode correctly, SDK preset effects are rendered, and suspended games are recognized. Lab captures also export as `.chroma`.
- Unified UI signal colours and lamp shapes, and gave faders click-to-type values, reset, fine adjustment and page-scroll pass-through.

## v0.1.2

- Reduced idle CPU work in dispatch, capture, tray handling, audio, gestures, and previews; Python workers start on demand without dropping the first action.
- Improved renderer selection and fallback, and added an opt-in WGPU renderer. OpenGL remains the default after higher WGPU idle use in local profiling.
- Hardened Razer replies, HID++ adoption, and device write verification; fixed intercepted-key release and sniper DPI restoration on the correct mouse.
- Serialized profile updates and migration across processes; unresolved recovery errors now stop loading or startup.
- Hardened the Windows lifecycle: autostart runs without administrator privileges, native Chroma setup is automatic and recoverable, installer updates retain their setup lifecycle, and v0.1.1 config migrates forward.
- Safe mode now starts with input disarmed and device writes paused.
- Bounded integration clients, validated OBS input, fixed Chroma RGB decoding, and made Linux rollback manifest-aware.

## v0.1.1

- Fixed prerelease update discovery and authenticated downloads for private repositories.
- Corrected installation, platform, troubleshooting, and macro documentation.

## v0.1.0 (public beta mk1)

- Initial Windows tray app and CLI for Razer device control, profiles, remaps, lighting, macros, and audio.
- Shipped with a disarmed input gate and read-back-verified device writes.
