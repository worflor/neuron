# neuron: state of the project

v0.1.0 is a Windows beta. This page tracks what has been exercised on hardware, what is implemented but needs more use, and what is still planned. The [README](../README.md) has the device-write ledger.

| grade | meaning |
|---|---|
| 🟢 solid | Daily-driven or directly tested on the named hardware. |
| 🟡 works, polishing | Usable, with recent changes or rough edges still being exercised. |
| 🟠 under-tested or partial | Implemented in part or not yet exercised enough to rely on broadly. |
| ⚪ planned | No working implementation yet. |

## Feature status

| area | grade | evidence and limits |
|---|---|---|
| Device control | 🟢 solid | DPI, polling, brightness, battery, scroll-stage selection and other reads are used on a Naga V2 Pro and BlackWidow Chroma V2. Implemented writes verify the device's read-back. Unconfirmed opcodes stay gated. |
| Naga side plates | 🟢 solid | The mouse reports plate swaps; Neuron debounces them and scopes binds to the seated plate. Detaching clears that layer. |
| Trigger → action dispatch | 🟢 solid | Binds, HyperShift layers and app-focus rules use one tested engine. |
| Arm gate | 🟢 solid | The process starts disarmed. Tests cannot arm input; device writes and synthesized input have separate controls. |
| Audio control | 🟢 solid | Mute, gain and output switching use the OS mixer. Also tested with the BlackShark V2 and its included USB sound card, and the Seiren V3 Mini; these have no extra device-specific Neuron controls. |
| Synapse import, purge and discover | 🟢 solid | Plaintext export import, Windows purge and device discovery are implemented. Encrypted Synapse cloud profiles are not supported. |
| Notifications | 🟢 solid | Visual cards and optional audio confirm committed changes. |
| Profiles | 🟡 works, polishing | Capture, apply, rename and app-based switching are implemented. A profile groups settings, lighting and binds; a recent pass fixed lifecycle edges that need continued use. |
| Device adoption | 🟡 works, polishing | Capability probing synthesizes definitions for unfamiliar `razer_report` devices. The Naga V2 Pro and BlackWidow Chroma V2 are hardware-verified; other families need device-specific checks. |
| Lighting | 🟡 works, polishing | Firmware effects, per-key frames, pattern × spectrum layers, and live data layers are implemented. The newer layer editing and data feeds need more everyday use. |
| Macros and beacons | 🟡 works, polishing | Warm BOUND and RAW CPython workers, ask/notify, persistent state, macro composition and a source-preserving block editor are implemented. API documentation and long-run ergonomics need work. RAW Python is unsandboxed. |
| Main GUI | 🟡 works, polishing | The Slint window uses GPU rendering with a software fallback. The lighting page has received the most design work. Windows overlay instruments still compose pixels on the CPU. |
| Reliability and recovery | 🟡 works, polishing | Flight recorder, crash log and read-only diagnostics are wired. Windows auto-relaunch is implemented but has not been exercised by a real crash or hang. |
| Onboard memory | 🟠 partial | Storage accounting and volatile writes work. Full onboard profile-slot persistence and scroll feel-curves are not complete. |
| Spellweaving at scale | 🟠 under-tested | Gesture recognition and radial dispatch work. Large glyph vaults and fully populated radial menus need sustained use. |
| Instruments | 🟠 uneven | Teleport, tether, whiteboard, glance, window actions, dial, knockback and control are present at different maturity levels. The whiteboard cannot save ink to disk yet. |
| CLI and headless daemon | 🟠 under-tested | The installed Windows CLI has driven real hardware and shares the GUI's core engine. Its broad command surface and daemon path have had less day-to-day use. |
| Protocol host | 🟠 early | Chroma and OpenRGB input paths reach the lighting arbiter. Ownership controls and teardown are still being hardened. Richer Chroma interpretation and dynamic redraw are planned. |
| Momentary mic | 🟠 under-tested | Press holds mute and release restores it. Config swaps, worker respawn and exit release held state; the path has had limited real-world use. |
| Install and update | 🟡 works, polishing | The Windows package was installed into a clean folder, checked for runtime DLLs, and used to drive hardware through the installed CLI. Linux update logic has synthetic install, rollback and checksum tests, but no released Linux package or native installation run. |
| Linux HID | 🟠 hardware-unverified | The hidraw transport builds and passes local tests under WSL2. No Razer device has exercised its feature-report path on Linux. |
| Linux GUI | 🟠 partial | The source builds a Slint window with device settings, lighting editor, GTK tray and best-effort hotkeys. Live input, audio and overlay work is on [`codex/linux-runtime-parity`](https://github.com/worflor/neuron/tree/codex/linux-runtime-parity). Its overlay does not yet match Windows, and its input path needs a native desktop and hardware run. v0.1.0 has no Linux download. |
| macOS | ⚪ planned | No backend or release build. |

For device-write evidence and feature gates, see the [README ledger](../README.md#honesty-proven-gated-absent). If a feature marked solid breaks, please [report the device and steps to reproduce](https://github.com/worflor/neuron/issues).