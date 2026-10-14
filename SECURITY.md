# security

neuron changes device settings, sends input and runs code you choose. that's real authority, so here's what it can and can't promise.

## what the surface actually is

**input is arm-gated.** the process starts disarmed. a normal GUI or daemon launch arms Neuron's live binds once dispatch starts; `--safe` does not. turning input off is immediate, and manually turning it back on requires confirmation. tests can't arm real input. this gate applies to Neuron-managed actions, not unrestricted RAW Python or other programs running under your account.

**device settings are checked against hardware.** supported setters with matching getters (such as DPI and polling) read back the result and fail on a mismatch. live lighting frames validate protocol replies, not the physical colour of every LED. unverified writes are gated or named as exceptions in the protocol ledger. settings are volatile by default where supported; `scroll` stores its stage onboard unless you use `--volatile`. Neuron needs no Razer SDK or additional kernel driver for these controls.

**neuron-host opens local surfaces, and every one of them is reachable by anything already running on your machine.** here's the complete list:

- **the OpenRGB protocol server (port 6742) and the Chroma REST face (port 54235)** bind loopback only. they exist so other apps on *your* machine can push into your lighting, and they're unauthenticated by design, the same as the tools they're compatible with.
- **the native Chroma face is shared memory, not a socket.** games reach Razer's SDK through `Global\` shared-memory objects. creating those sections requires `SeCreateGlobalPrivilege`. The packaged `neuron-chroma-broker` runs from a protected Program Files directory at high integrity and only creates and holds the fixed Chroma sections. The limited tray process adopts them and owns parsing, arbitration and device output. The broker has no macro, input, network or HID path. The sections grant **full access to Everyone** (`D:(A;;GA;;;WD)`), because that is what games expect to open. Without the broker, Chroma REST remains available.

  that permission isn't a choice i get to make: the game uses `FILE_MAP_ALL_ACCESS`, which includes write-DACL/write-owner rights. a tighter descriptor fails the open and the game can't paint. neuron uses fixed section sizes and bounds-checked parsing. don't elevate the whole app to make native Chroma work; RAW Python would inherit that authority too.
- **a held loopback port (47615)** marks "a neuron host already owns this machine" so two instances never both write to a device. it's never accepted on, so it carries no data.
- **an outbound connection to OBS Studio (`127.0.0.1:4455`)**, and only if you turn it on (it's off by default). it runs both ways: macros can drive scenes, streaming and recording, and OBS's events come back in and can fire your `on_obs_*` hook macros. so whatever is answering on that port can trigger those macros. set an obs-websocket password if that matters to you.

treat anything with local code execution as able to paint your keyboard. none of these surfaces synthesize input, write to a device beyond lighting, or touch the filesystem. host integration can be turned off entirely with `NEURON_HOST=0`.

**macros have two explicit authority tiers.** new macros are **BOUND** by default: ordinary Python computation runs in a dedicated interpreter that starts on input arming or an editor action, then stays warm. Input, clipboard, focus, device/audio/OBS effects and persistent macro state go through Neuron's Rust broker. BOUND removes ambient file/process/network/native-FFI access from the supported Python surface and Rust re-checks the arm/mock gate before effectful broker actions land. BOUND is *policy containment*, not a promise that CPython is a hostile-code security sandbox; don't treat untrusted code as safe merely because it is BOUND.

**RAW is still full Python on purpose.** one source-owned compiler line — `# neuron: raw` — moves that macro into a separate unrestricted warm CPython process with normal `ctypes` / `subprocess` / socket / filesystem authority. the GUI toggle edits that exact line; there is no hidden authority preference. pre-BOUND macros are stamped RAW once during migration so an upgrade does not silently take power away. RAW code can deliberately bypass Neuron's helpers and arm gate through ambient APIs, so read RAW macro source exactly as you would any other program you run.

BOUND and RAW never share an interpreter. a RAW crash cannot corrupt BOUND runtime state, and a BOUND macro cannot invoke a RAW macro as an authority tunnel; RAW may invoke BOUND, which still executes inside BOUND. the process boundary remains a reliability boundary for RAW and an important structural policy boundary for BOUND, but neither tier is marketed as adversarial-code isolation.

**no account, no telemetry, no Neuron cloud.** config is plain TOML on your disk and Neuron itself does not phone home. the built-in runtime network surface is the opt-in loopback OBS connection above; RAW macros can of course open arbitrary network connections because they are ordinary Python. the build script also fetches and sha256-verifies the bundled CPython tarball at build time.

## reporting something

use GitHub's private vulnerability reporting on this repo (Security → Report a vulnerability). that goes to me directly and stays private until there's a fix. please don't open a public issue for anything that lets code on a machine escalate through neuron.

it's one person on one desk, so i can't promise a response window, but i read them. include what you'd want if you were fixing it: version or commit, OS, device, and the shortest repro you have.
