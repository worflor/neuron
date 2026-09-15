# security

neuron injects input, writes to your hardware, and runs code you wrote. that's a real attack surface, so here's the honest shape of it.

## what the surface actually is

**input injection is arm-gated.** every synthesised keystroke, click, and process spawn goes through one process-wide switch that starts *disarmed*. only the running daemon or GUI ever flips it on, and arming takes a deliberate confirm. tests can't arm it (there's a test whose only job is enforcing that). if that gate can be flipped by anything other than a deliberate user action, that's a vulnerability and i want to hear about it.

**device writes verify themselves.** writes default to volatile (`NOSTORE`), read back the matching getter, and hard-error on a mismatch rather than reporting a silent success. anything without a trusted opcode refuses instead of guessing. no kernel driver, no vendor SDK, just HID feature reports on an access-zero handle.

**neuron-host opens local surfaces, and every one of them is reachable by anything already running on your machine.** here's the complete list:

- **the OpenRGB protocol server (port 6742) and the Chroma REST face (port 54235)** bind loopback only. they exist so other apps on *your* machine can push into your lighting, and they're unauthenticated by design, the same as the tools they're compatible with.
- **the native Chroma face is shared memory, not a socket.** games reach Razer's SDK through `Global\` shared-memory objects, so to stand in for that server neuron creates those objects with a security descriptor granting **full access to Everyone** (`D:(A;;GA;;;WD)`), because that is what games expect to open. it only comes up when neuron runs elevated (creating `Global\` objects needs `SeCreateGlobalPrivilege`). any local process, including one running as a different user, can paint through it.
- **a held loopback port (47615)** marks "a neuron host already owns this machine" so two instances never both write to a device. it's never accepted on, so it carries no data.
- **an outbound connection to OBS Studio (`127.0.0.1:4455`)**, and only if you turn it on (it's off by default). it runs both ways: macros can drive scenes, streaming and recording, and OBS's events come back in and can fire your `on_obs_*` hook macros. so whatever is answering on that port can trigger those macros. set an obs-websocket password if that matters to you.

treat anything with local code execution as able to paint your keyboard. none of these surfaces synthesize input, write to a device beyond lighting, or touch the filesystem. host integration can be turned off entirely with `NEURON_HOST=0`.

**macros are unsandboxed on purpose.** the bundled CPython runs *your* macro files as a subprocess with full `ctypes` / `subprocess` / socket / filesystem access. that's the feature, not an oversight: a macro can do anything a program you ran can do. the friendly helpers respect the arm gate; raw `ctypes` past them does not. don't run a macro file you didn't read, exactly like any other script. the sidecar being a separate process is a *reliability* boundary (a segfaulting macro can't take the app down), not a security one.

**no network, no account, no telemetry.** config is plain TOML on your disk. nothing phones home. nothing leaves your machine at runtime: the only runtime connection is the opt-in loopback link to OBS above. the only thing that touches the internet at all is the build script fetching (and sha256-verifying) the CPython tarball at build time.

## reporting something

use GitHub's private vulnerability reporting on this repo (Security → Report a vulnerability). that goes to me directly and stays private until there's a fix. please don't open a public issue for anything that lets code on a machine escalate through neuron.

it's one person on one desk, so i can't promise a response window, but i read them. include what you'd want if you were fixing it: version or commit, OS, device, and the shortest repro you have.
