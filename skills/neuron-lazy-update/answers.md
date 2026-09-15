# Answering questions about neuron

Answer from these sources, and say which one you used. If they don't cover the question, say so.
Don't fill the gap with a guess.

A release install includes `README.md` and `SECURITY.md` next to the exes. Everything else is in the
repository: https://github.com/worflor/neuron

| the question is about… | look here |
|---|---|
| what neuron is, what it does, install basics | `README.md` |
| whether a feature works yet, and how well | [docs/STATUS.md](https://github.com/worflor/neuron/blob/main/docs/STATUS.md) |
| which device writes are proven, gated, or unsupported | `README.md`, section *honesty: proven, gated, absent* |
| how a feature behaves in detail (lighting, binds, spellweaving, macros, the app pages) | [docs/GDD.md](https://github.com/worflor/neuron/blob/main/docs/GDD.md) |
| how it's built internally | [docs/TDD.md](https://github.com/worflor/neuron/blob/main/docs/TDD.md) |
| the lighting integrations hub (OpenRGB, Chroma, OBS) | [docs/PROTOCOL-HOST.md](https://github.com/worflor/neuron/blob/main/docs/PROTOCOL-HOST.md) |
| what's exposed on the machine, macro safety, reporting a vulnerability | `SECURITY.md` |
| a command's exact options | `neuron.exe <command> --help` |
| contributing | [CONTRIBUTING.md](https://github.com/worflor/neuron/blob/main/CONTRIBUTING.md) |

## Things people ask a lot

Short answers you can give directly. Check the linked source if the user wants more.

- **Does it need an account or send data anywhere?** No account, no telemetry, nothing sent at
  runtime. The one optional network link is to OBS on the same machine. (`SECURITY.md`)
- **Can I use it with Synapse installed?** Not on the same device at the same time. neuron can
  import Synapse settings and remove Synapse. (`README.md`, GDD *life after synapse*)
- **Which devices work?** Razer devices that speak `razer_report`. Two are hardware-verified:
  Naga V2 Pro and BlackWidow Chroma V2. Others should adopt themselves automatically, but aren't
  verified. (`docs/STATUS.md`)
- **Linux or Mac?** Not yet. Windows only. (`docs/STATUS.md`)
- **Why does Windows warn when I run it?** It isn't code-signed. The release carries a provenance
  attestation you can verify instead. (`README.md`, *get it*)
- **Where are my settings?** In the install folder next to the exes, or in `%LOCALAPPDATA%\neuron`
  if the install folder isn't writable.

## Reporting a bug

Help the user file a good report:

1. Open https://github.com/worflor/neuron/issues/new?template=bug_report.md
2. Include: the device and its PID (`neuron.exe list` shows it), the Windows build (`winver`), what
   happened, and what they expected.
3. For device or lighting problems: open the SYSTEM page in the app, run the diagnostics bench
   (nine read-only checks, always safe), and paste the output.
4. If `neuron-crash.log` exists in the config folder, attach it.

A security problem does **not** go in a public issue. Use the private report link in `SECURITY.md`.
