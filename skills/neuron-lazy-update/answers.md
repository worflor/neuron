# Answering questions about neuron

Answer from the installed build and the sources below, and name the evidence
used. State any uncertainty that affects the answer.

A release install includes `README.md` and `SECURITY.md` next to the exes. Everything else is in the
repository: https://github.com/worflor/neuron

| the question is about… | look here |
|---|---|
| what neuron is, what it does, install basics | `README.md` |
| feature availability and verification | [docs/STATUS.md](https://github.com/worflor/neuron/blob/main/docs/STATUS.md) |
| which device writes are proven, gated, or unsupported | `README.md`, section *honesty: proven, gated, absent* |
| how a feature behaves in detail (lighting, binds, spellweaving, macros, the app pages) | [docs/GDD.md](https://github.com/worflor/neuron/blob/main/docs/GDD.md) |
| how it's built internally | [docs/TDD.md](https://github.com/worflor/neuron/blob/main/docs/TDD.md) |
| the lighting integrations hub (OpenRGB, Chroma, OBS) | [docs/reference/PROTOCOL-HOST.md](https://github.com/worflor/neuron/blob/main/docs/reference/PROTOCOL-HOST.md) |
| what's exposed on the machine, macro safety, reporting a vulnerability | `SECURITY.md` |
| a command's exact options | `neuron.exe <command> --help` |
| contributing | [CONTRIBUTING.md](https://github.com/worflor/neuron/blob/main/CONTRIBUTING.md) |

## Things people ask a lot

Use the linked evidence to answer for the user's version and hardware.

- **Does it need an account or send data anywhere?** No account, cloud or telemetry. The optional
  OBS link stays on the same machine; optional OpenRGB and Chroma listeners accept local clients.
  RAW Python macros may use the network if their author writes them to. (`SECURITY.md`)
- **Can I use it with Synapse installed?** Not on the same device at the same time. neuron can
  import Synapse settings and remove Synapse. (`README.md`, GDD *life after synapse*)
- **Which devices work?** Check the device-specific grades and hardware evidence in
  `docs/STATUS.md`. Distinguish detection, verified reads, verified writes, and audio support.
- **Linux or Mac?** Check the release's platform assets, README build instructions, and
  `docs/STATUS.md`. Distinguish a released download, a source build, and verification on hardware.
- **Why does Windows warn when I run it?** Check the selected package's signing information,
  `SHA256SUMS.txt`, `SOURCE.txt`, and published provenance. Explain what that evidence verifies.
- **Where are my settings?** In the install folder next to the exes, or in `%LOCALAPPDATA%\neuron`
  if the install folder isn't writable.

## Something broken, or a bug to report

Follow [issues.md](issues.md): research it, answer, and offer to draft an issue only when it's a
real, unreported problem. A security problem never goes in a public issue. Use the private report
link in `SECURITY.md`.
