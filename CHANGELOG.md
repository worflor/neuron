# Changelog

## Unreleased

- The device registry is one shared snapshot that reloads itself when definition files change. Device discovery, event arming, macro-key handling, macro device verbs and the background workers all read it, so a device adopted or edited mid-session reaches every part of the app without a restart. Previously each subsystem kept its own copy (macro device verbs kept the one from first use), and the vitals and scan ticks re-parsed every file each time.
- Keyboards held in driver mode for their macro keys keep their Fn layer and media keys. Driver mode hands Fn to the host, so Fn combos had stopped working while Neuron ran. The BlackWidow Chroma V2's Fn row comes from its definition: volume, media, sleep, and Fn+F10 toggles the keyboard's own game mode with read-back. Any keyboard's media-key reports (BlackWidow V3 and V4 class) now act as media keys. Your own binds on those keys still win; binding Fn itself turns the default layer off. New `game-mode` action.
- A HyperShift layer with a hold key is held only by that key. Pressing a shifted key alone used to hold its layer and fire the shifted action; it now types normally. While the hold key is down, the keyboard hook takes the shifted key, so it no longer also types.
- Binds on one device's key that its firmware can't perform (a key on a BlackWidow, a side button on another mouse) work. The hook used to swallow the key and then wait for Raw Input to name the device, which a swallowed key never reaches, so the action never fired and the key typed late. The hook now decides the device itself: a key only one connected device can send is exact, and a key several keyboards share goes to the one typed on last.
- When a device's button functions can't be verified at start or arm, for example a sleeping wireless mouse, input no longer stays in safe mode for the rest of the session. The status bar shows "arming" with the reason, retries every few seconds, and arms once every device is back to stock. Choosing safe yourself cancels it.
- Device files in the run root may only carry the side-plate and Fn binds a built-in definition ships, so a shared file cannot attach a command to a key.
- Macro sidecars are no longer killed when a busy machine delays their arming acknowledgement. Arming no longer waits, a fresh sidecar (which starts disarmed) only waits when it must confirm being armed, and only a disarm still waits briefly and kills a sidecar that does not confirm it.

## v0.1.7

### Security
- BOUND macros run behind a runtime wall that refuses file, process, network, native-library and new-module access by any route, closing an escape through string-based reflection. BOUND ignores `PYTHONPATH`, `PYTHONHOME` and user site packages.
- `neuron apply` and `profile import` list and refuse RAW macros, command-running binds, path screenshots and onboard-memory profiles a file adds, unless `--allow-code` is passed.
- Device definition files are bounded at load by Neuron's protocol knowledge: command opcodes per name, lighting writes, vendor id and settle wait. Refused and unparseable files are reported.
- The Chroma REST server refuses requests whose `Host` is not a loopback name.
- The updater validates `-Version`, downloads only from this repository's releases and verifies the setup program's attestation. The bundled CPython digest is pinned in source, and CI release builds re-derive the embedded runtime from the pinned tarball.
- Elevated helpers no longer copy or load from user-writable folders.

### Macros
- BOUND macros can use `signal` and the OBS helpers; `obs_request` is RAW-only.
- Effect helpers return `True` or the host's confirmation on success and a falsy `"[reason]"` otherwise, in both modes. BOUND input helpers previously returned `"ok"`.
- `datetime.strptime` works in BOUND macros. Dry runs no longer change live Signal lighting. Parse and check wait up to 10 s, so a slow parse no longer restarts the Python session.
- Added the macro API reference (`docs/reference/MACROS.md`).

### Devices
- Generated definitions for DPI-less keyboards on the 0x0F lighting protocol (BlackWidow V3, V4 X) target the whole board and the backlight. Unverified on those boards. Older generated files of that shape are rebuilt on the next adoption, with a `.v1.bak` backup.
- Generated device files no longer record round-trips or answers that were never measured.
- The device report form asks about Windows Dynamic Lighting and accepts the generated device file.

### Reliability
- Sleep and Modern Standby no longer cause false stall warnings. Stall dumps record each worker's thread state, wait reason and CPU use.
- Quitting from the tray no longer panics during teardown.
- Chroma broker updates stage and verify the replacement before stopping the working copy and restore it on failure; checked through real UAC. Uninstall keeps unresolved recovery state.

### Setup documents
- `neuron apply` rejects unknown sections and names the section and folder when a write fails.

## v0.1.6 (tagged, not published; included in v0.1.7)

- Preserved lighting ownership across host recovery and fixed OpenRGB single-LED updates after surface resizing.
- Refreshed macro-key eligibility after keyboard adoption, with held-key state shared correctly across sibling collections and aliases.
- Rejected malformed Razer discovery replies and incomplete HID++ replies, while preserving Windows' padded short-report handling.
- Added offline Windows copy inventory (`neuron-update.ps1 -Action status`, `-Json` for agents), separating the running repo build, installed checkpoint, startup target and config paths.
- Repository builds, installers and uninstall preserve other copies' startup target and the shared Chroma broker; `release.ps1 -UseAtStartup` makes reassignment explicit. User configuration stays separate from payload removal.
- Already-current updates skip redundant downloads and backups; updater scratch directories are cleaned up.

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
