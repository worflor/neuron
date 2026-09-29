# chroma_shm_data

Binary assets the native-Chroma codec (`../chroma_shm.rs`) compiles in with
`include_bytes!`. They are tracked on purpose — the tests decode **real** bytes,
not synthesized ones, so the parser stays honest against what a live game
actually writes. Small and stable (a few captured frames + one lookup table).

| file | role | used by |
| --- | --- | --- |
| `keystream.bin` | 512-byte XOR de-obfuscation table (runtime asset, not test-only) | `KEYSTREAM`, the frame decoder |
| `overwatch-keyboard-section.bin` | a keyboard section from a suspended Overwatch (effect 13) | `SUSPENDED_KEYBOARD`: suspended is read as suspended |
| `overwatch-device02-section.bin` | the mouse section from the same suspended session | `SUSPENDED_MOUSE` |
| `overwatch-live-keyboard.bin` | a keyboard section mid-match: a custom 6x22 grid (Tracer, match 2) | `LIVE_KEYBOARD` grid + corruption tests |
| `overwatch-live-mouse.bin`, `-headset.bin`, `-mousepad.bin`, `-chroma-link.bin` | the other classes from the same instant, each a static colour (the match ambient) | per-class layout tests |
| `overwatch-session-table.bin` | the live session table | `OW_SESSION_TABLE` parser test |
| `overwatch-app-registry.bin` | the connected-app registry | `OW_APP_REGISTRY` parser test |
| `overwatch-roster.bin` | the device roster | `OW_ROSTER` parser test |
| `overwatch-keyboard-scene.bin` | 750 decoded 6x22 keyboard frames from five windows of real play (see below) | `chroma_scene` fixture test |

`overwatch-keyboard-scene.bin` is a delta stream: `NCS1`, `u16` cell count, then per frame
`u32` ms since capture start, `u16` changed-cell count, and that many `(cell, r, g, b)` byte
quads (the first frame lists every cell). Cut from a 600 s read-only capture of a real
session on 2026-09-28: hero select at 18.8-22.4 s (one ult wave), match 1 at 49.0-53.6 s
(four green pulses), match 2 at 388.0-391.9, 408.0-411.7 and 433.4-436.9 s (one ult wave
each).

Captured live (Overwatch on SDK 3.37, 2026-07-03). Raw exploratory dumps from
that reverse-engineering live under `docs/chroma-shm-capture/`, which is
git-ignored — only the handful the codec/tests actually need were promoted here.

The Windows `bridge` test `synthetic_mapping_snapshot_decodes_frame` seeds a
uniquely named `Local\` mapping, writes the captured keyboard section as a
synthetic game frame, opens it through a second mapping handle, and exercises
the production volatile snapshot and decode path. Run it with:

```powershell
cargo test -p neuron-host --features bridge synthetic_mapping_snapshot_decodes_frame -- --nocapture
```

It never claims the `Global\` Chroma names or wears the live arbitration mask.
