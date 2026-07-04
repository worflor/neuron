# chroma_shm_data

Binary assets the native-Chroma codec (`../chroma_shm.rs`) compiles in with
`include_bytes!`. They are tracked on purpose — the tests decode **real** bytes,
not synthesized ones, so the parser stays honest against what a live game
actually writes. Small and stable (a few captured frames + one lookup table).

| file | role | used by |
| --- | --- | --- |
| `keystream.bin` | 512-byte XOR de-obfuscation table (runtime asset, not test-only) | `KEYSTREAM`, the frame decoder |
| `overwatch-keyboard-section.bin` | a keyboard device section as Overwatch painted it | `OW_KEYBOARD` header/grid tests |
| `overwatch-device02-section.bin` | a second device class (type 0x02) from the same frame | `OW_DEVICE_02` class test |
| `overwatch-session-table.bin` | the live session table | `OW_SESSION_TABLE` parser test |
| `overwatch-app-registry.bin` | the connected-app registry | `OW_APP_REGISTRY` parser test |
| `overwatch-roster.bin` | the device roster | `OW_ROSTER` parser test |

Captured live (Overwatch on SDK 3.37, 2026-07-03). Raw exploratory dumps from
that reverse-engineering live under `docs/chroma-shm-capture/`, which is
git-ignored — only the handful the codec/tests actually need were promoted here.
