# Neuron license

Copyright (c) 2026 Woflo Labs.

Permission is granted to copy and distribute this Work Notice verbatim with
Neuron. A modified Work Notice may accompany a permitted modified version of
Neuron, but it must accurately identify the licenses and applicable notices and
must not imply endorsement by Woflo Labs.

Neuron is built in public, but not every part of it is released under the same
terms. Most of the project is GPL; a small reusable research core uses the Woflo
Labs Community Source License. This file is the controlling Work Notice and the
exact boundary between them.

**Work:** Neuron, including the original source code, documentation, device
definitions, tests, interface material, and Woflo research components in this
repository.

**Repository:** `https://github.com/worflor/neuron`

**Licensor:** Michael Bickford ("Woflo"), publishing as Woflo Labs.

## Most of Neuron: GPL-3.0-or-later

Unless a path is listed in the next section or carries its own third-party
notice, original material in this repository is licensed under the GNU General
Public License, version 3 or later, with the Neuron-Woflo Research Components
Exception.

The legal instruments in this `LICENSE.md`, in `LICENSES/**`, and in
`crates/engram/LICENSE.md` are not project source or documentation covered by a
project-code license. Each is governed by the copying permission stated in its
own text.

- [GNU GPL version 3](LICENSES/GPL-3.0-or-later.txt)
- [Neuron-Woflo Research Components Exception 1.0](LICENSES/NEURON-WOFLO-EXCEPTION-1.0.md)

Neuron-specific code that calls, displays, tests, or integrates a protected
research component remains GPL-covered unless its own path is listed below.

## Woflo research components: WLCSL-1.0

These paths are licensed under the
[Woflo Labs Community Source License 1.0](LICENSES/WLCSL-1.0.md):

- `crates/engram/**`, except the legal instruments in its `LICENSE.md` as stated
  in that file
- `crates/neuron-core/src/glyph.rs`
- `crates/neuron-core/src/logos.rs`
- `crates/neuron-core/src/gwyph.rs`

In these patterns, `*` matches within a single directory and `**` matches a whole
tree. The boundary is file-level: a file is covered by WLCSL-1.0 only if it
matches an entry in the list above. Covered files whose format allows comments
also carry a `LicenseRef-WLCSL-1.0` notice, and GPL-covered files carry a
GPL-3.0-or-later notice naming the exception, but those notices are a
convenience for readers and tools. This list controls.

## Combined Neuron builds

The Neuron-Woflo exception permits the GPL-covered parts of Neuron to be linked,
combined, and distributed with the WLCSL-covered research components. Each part
keeps its own license, and a combined distribution must satisfy the GPL, the
exception, and the applicable terms for the research components.

The exception permits the combination; it does not convert the research
components to GPL or waive their conditions. A GPL-only fork may remove the
protected components and the code paths that require them, but this notice does
not promise that such a configuration already compiles unchanged.

## Contributions

Contributions accepted after this notice follow the
[Woflo Labs Contributor Agreement 1.0](LICENSES/CONTRIBUTOR-AGREEMENT-1.0.md)
and the acceptance process in [CONTRIBUTING.md](CONTRIBUTING.md).

A contribution accepted into a GPL-covered path is promised back publicly under
GPL-3.0-or-later with the Neuron-Woflo exception. A contribution accepted into
one of the research paths above is promised back publicly under WLCSL-1.0.

## Patents, third-party material, and names

Some Woflo research components may implement subject matter described in
pending patent applications. WLCSL-1.0 grants patent rights only for the uses
stated there. "Patent pending" does not mean that a patent has issued
or that any particular claim will be allowed.

Dependencies, referenced projects, vendor material, and files carrying their
own notices are not relicensed here; their own terms continue to apply.
Third-party material that ships inside a Neuron build, including the embedded
CPython runtime, is inventoried with its required notices in
[THIRD-PARTY-NOTICES.md](THIRD-PARTY-NOTICES.md).

Neither license grants trademark rights in Woflo Labs, Neuron, Whisper, Engram,
Glyph, Logos, or related names and visual identities, except the descriptive
attribution allowed by the applicable license.

Woflo Labs is the publishing and independent research identity of one
individual, not a separate incorporated entity as of this notice. The Licensor
is identified above.
