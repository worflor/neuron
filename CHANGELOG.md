# Changelog

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
