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

- **Does it need an account or send data anywhere?** No Neuron account, cloud or telemetry.
  OpenRGB, Chroma and the optional OBS link are local integrations. RAW Python macros can use
  the network like ordinary Python. (`SECURITY.md`)
- **Can I replace Synapse?** The maintainer does on the Naga V2 Pro and BlackWidow Chroma V2.
  Other devices need their own checks; don't promise compatibility from detection alone.
  Synapse and Neuron shouldn't control the same device at once. (`README.md`, `docs/STATUS.md`)
- **Which devices work?** Check `docs/STATUS.md` for verified hardware and feature grades.
  To help test another device, use the [device report](https://github.com/worflor/neuron/issues/new?template=device_report.yml)
  or the optional [`neuron-compat` skill](../neuron-compat/SKILL.md).
- **Linux or Mac?** Windows is the released platform. Linux app and CLI build from source,
  but have no released package or native Razer HID verification; macOS has no backend.
- **Why does Windows warn when I run it?** The Windows builds are unsigned. Check the
  release's `SHA256SUMS.txt`, `SOURCE.txt` and provenance to verify the download.
- **Where are my settings?** Installer/source builds use `%LOCALAPPDATA%\neuron` on Windows.
  A portable ZIP marked with `portable.flag` keeps its setup beside the executables.
  `NEURON_RUN_DIR` overrides either location. (`README.md`, *where your config lives*)

## Something broken, or a bug to report

Follow [issues.md](issues.md): research it, answer, and offer to draft an issue only when it's a
real, unreported problem. A security problem never goes in a public issue. Use the private report
link in `SECURITY.md`.
