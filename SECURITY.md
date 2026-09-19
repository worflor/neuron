# security

neuron injects input, writes to your hardware, and runs code you wrote. that's a real attack surface, so here's the honest shape of it.

## what the surface actually is

**input injection is arm-gated.** every synthesised keystroke, click, and process spawn goes through one process-wide switch that starts *disarmed*. only the running daemon or GUI ever flips it on, and arming takes a deliberate confirm. tests can't arm it (there's a test whose only job is enforcing that). if that gate can be flipped by anything other than a deliberate user action, that's a vulnerability and i want to hear about it.

**device writes verify themselves.** writes default to volatile (`NOSTORE`), except `neuron scroll <stage>`, which stores onboard by default like synapse (`--volatile` opts out). every write reads back the matching getter and hard-errors on a mismatch rather than reporting a silent success. anything without a trusted opcode refuses instead of guessing. no kernel driver, no vendor SDK, just HID feature reports on an access-zero handle.

**neuron-host opens local surfaces, and every one of them is reachable by anything already running on your machine.** here's the complete list:

- **the OpenRGB protocol server (port 6742) and the Chroma REST face (port 54235)** bind loopback only. they exist so other apps on *your* machine can push into your lighting, and they're unauthenticated by design, the same as the tools they're compatible with.
- **the native Chroma face is shared memory, not a socket.** games reach Razer's SDK through `Global\` shared-memory objects, so to stand in for that server neuron creates those objects with a security descriptor granting **full access to Everyone** (`D:(A;;GA;;;WD)`), because that is what games expect to open. it only comes up when neuron runs elevated (creating `Global\` objects needs `SeCreateGlobalPrivilege`). any local process, including one running as a different user, can paint through it.

that permission isn't a choice i get to make — a game opens these with `FILE_MAP_ALL_ACCESS`, which includes the standard write-DACL/write-owner rights, so a tighter descriptor fails the open and the game never paints. what i can control is what hostile bytes are worth, so: neuron creates every section itself at a fixed size, the attacker owns the contents and never the length, every parse loop is bounds-checked against that length, and the one index derived from the data (the XOR phase) is masked and clamped, with a test that exhausts all 128 reachable values and asserts the decode path neither indexes out of range nor panics. the honest ceiling on abusing this is painting your lighting or denying the bridge — not code execution in the elevated process. if you find a path past that, it's exactly what i want to hear about.
- **a held loopback port (47615)** marks "a neuron host already owns this machine" so two instances never both write to a device. it's never accepted on, so it carries no data.
- **an outbound connection to OBS Studio (`127.0.0.1:4455`)**, and only if you turn it on (it's off by default). it runs both ways: macros can drive scenes, streaming and recording, and OBS's events come back in and can fire your `on_obs_*` hook macros. so whatever is answering on that port can trigger those macros. set an obs-websocket password if that matters to you.

treat anything with local code execution as able to paint your keyboard. none of these surfaces synthesize input, write to a device beyond lighting, or touch the filesystem. host integration can be turned off entirely with `NEURON_HOST=0`.

**macros have two explicit authority tiers.** new macros are **BOUND** by default: ordinary Python computation runs in a dedicated warm interpreter, while input, clipboard, focus, device/audio/OBS effects and persistent macro state go through Neuron's Rust broker. BOUND removes ambient file/process/network/native-FFI access from the supported Python surface and Rust re-checks the arm/mock gate before effectful broker actions land. BOUND is *policy containment*, not a promise that CPython is a hostile-code security sandbox; don't treat untrusted code as safe merely because it is BOUND.

**RAW is still full Python on purpose.** one source-owned compiler line — `# neuron: raw` — moves that macro into a separate unrestricted warm CPython process with normal `ctypes` / `subprocess` / socket / filesystem authority. the GUI toggle edits that exact line; there is no hidden authority preference. pre-BOUND macros are stamped RAW once during migration so an upgrade does not silently take power away. RAW code can deliberately bypass Neuron's helpers and arm gate through ambient APIs, so read RAW macro source exactly as you would any other program you run.

BOUND and RAW never share an interpreter. a RAW crash cannot corrupt BOUND runtime state, and a BOUND macro cannot invoke a RAW macro as an authority tunnel; RAW may invoke BOUND, which still executes inside BOUND. the process boundary remains a reliability boundary for RAW and an important structural policy boundary for BOUND, but neither tier is marketed as adversarial-code isolation.

**no account, no telemetry, no Neuron cloud.** config is plain TOML on your disk and Neuron itself does not phone home. the built-in runtime network surface is the opt-in loopback OBS connection above; RAW macros can of course open arbitrary network connections because they are ordinary Python. the build script also fetches and sha256-verifies the bundled CPython tarball at build time.

## reporting something

use GitHub's private vulnerability reporting on this repo (Security → Report a vulnerability). that goes to me directly and stays private until there's a fix. please don't open a public issue for anything that lets code on a machine escalate through neuron.

it's one person on one desk, so i can't promise a response window, but i read them. include what you'd want if you were fixing it: version or commit, OS, device, and the shortest repro you have.
