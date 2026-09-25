# Changelog

Neuron remains in beta mk1. Version numbers identify builds within mk1.

## v0.1.0

- Initial Windows release: tray app and CLI for Razer device control, profiles, remaps, lighting, macros, and audio.
- The input gate initialized disarmed; the normal GUI armed it when live dispatch started. Device writes defaulted to volatile settings, verified by read-back, with uncertain writes gated.
- Hardware exercised: Naga V2 Pro and BlackWidow Chroma V2 for device control; BlackShark V2 USB sound card and Seiren V3 Mini for audio.
- Linux and macOS had no release builds.

## v0.1.1

- Fixed updater discovery of prereleases and private-repository downloads through authenticated `gh`.
- Corrected installation, platform-status, troubleshooting, and macro documentation.

## v0.1.2 (unreleased)

- Fixed Win32 live-dispatch and spellweave capture wake loops that could spin at idle. Capture and tray/hotkey handling now wait for events; Python workers start on demand.
- Reduced work and allocations in audio-spectrum analysis, gesture recognition, and UI material previews.
- Added a software UI renderer option and an opt-in WGPU backend. FemtoVG/OpenGL remains the default; WGPU used more idle CPU and memory in local profiling.
- Rejected malformed or unrelated Razer replies. Tightened write read-back checks and gated brightness writes on devices without a matching getter. Restricted read-only HID++ adoption to supported pipe shapes.
- Replayed intercepted input and released held mapped keys on pause, disarm and remap reload. Tracked DPI provenance per physical device instance and pinned sniper restoration to the original unit.
- Serialized profile sidecar changes and legacy config migration across processes. Interrupted sidecar deletes recover; profile and bind loading stop on unresolved recovery conflicts. A failed legacy config migration stops startup.
- Changed Windows autostart to limited privileges. Safe mode now pauses writes from startup. Windows packages include a fixed-purpose broker for native Chroma shared memory; its protected installation is a separate administrator step. The updater refuses an old elevated tray task before replacing files.
- Capped local Chroma and OpenRGB clients, rejected malformed OBS JSON, and preserved post-install edits during Linux updater rollback.
- Corrected the Chroma shared-memory RGB API to return decoded colors and added a synthetic mapped-frame test using a captured game frame.

Runtime and hardware verification limits are tracked in [project status](docs/STATUS.md).
