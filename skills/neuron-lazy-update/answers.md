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

- **Does it need an account or send data anywhere?** No account, cloud or telemetry. The optional
  OBS link stays on the same machine; optional OpenRGB and Chroma listeners accept local clients.
  RAW Python macros may use the network if their author writes them to. (`SECURITY.md`)
- **Can I use it with Synapse installed?** Not on the same device at the same time. neuron can
  import Synapse settings and remove Synapse. (`README.md`, GDD *life after synapse*)
- **Which devices work?** Naga V2 Pro and BlackWidow Chroma V2 have hardware-verified controls.
  Other `razer_report` devices can be probed, but need their own hardware checks. BlackShark V2,
  its USB sound card, and Seiren V3 Mini have also been tested as audio devices. (`docs/STATUS.md`)
- **Linux or Mac?** v0.1.1 has no Linux download. The Linux CLI builds and passes local tests, and
  a partial GUI runs from source; the [runtime parity branch](https://github.com/worflor/neuron/tree/codex/linux-runtime-parity)
  has unfinished input, overlay, and audio work. No Razer hardware has verified the Linux HID or
  input paths. There is no macOS build. (`docs/STATUS.md`)
- **Why does Windows warn when I run it?** The binaries are not code-signed. Check
  `SHA256SUMS.txt`; `SOURCE.txt` identifies the source commit. The locally built Windows assets
  have no provenance attestation. (`README.md`, *get it*)
- **Where are my settings?** In the install folder next to the exes, or in `%LOCALAPPDATA%\neuron`
  if the install folder isn't writable.

## Something broken, or a bug to report

Follow [issues.md](issues.md): research it, answer, and offer to draft an issue only when it's a
real, unreported problem. A security problem never goes in a public issue. Use the private report
link in `SECURITY.md`.
