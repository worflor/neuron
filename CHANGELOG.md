# Changelog

## v0.1.2 (unreleased)

- Reduced idle CPU work in dispatch, capture, tray handling, audio, gestures, and previews; Python workers now start on demand.
- Added software and opt-in WGPU renderers. OpenGL remains the default after higher WGPU idle use in local profiling.
- Hardened Razer replies, HID++ adoption, and device write verification; fixed intercepted-key release and sniper DPI restoration on the correct mouse.
- Serialized profile updates and migration across processes; unresolved recovery errors now stop loading or startup.
- Replaced elevated autostart with a limited task, paused writes in safe mode, and added an administrator-installed Chroma shared-memory broker.
- Bounded integration clients, validated OBS input, fixed Chroma RGB decoding, and preserved local edits during Linux updater rollback.

## v0.1.1

- Fixed prerelease update discovery and authenticated downloads for private repositories.
- Corrected installation, platform, troubleshooting, and macro documentation.

## v0.1.0 (public beta mk1)

- Initial Windows tray app and CLI for Razer device control, profiles, remaps, lighting, macros, and audio.
- Shipped with a disarmed input gate and read-back-verified device writes.
