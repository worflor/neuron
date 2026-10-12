# Platform and connection comparisons

Use the same native evidence workflow on each machine. A Windows pass does not
verify Linux access, parsing, routing or output. Run only on a host the person has
made available for this investigation; otherwise give them the short command list
or a handoff for an agent running there. Do not make them install Rust or change
their hardware setup to submit an inventory.

## Linux / Fedora

Check the actual `neuron compat --help` before running commands. If the installed
build lacks the collector, use the updater route only when a matching supported
release is available. Do not imply that Linux has a released package or that a
source-only feature can be installed through the updater.

Collection retains hidraw report descriptors and available sysfs metadata locally.
Complete normalized fields are not yet available, so the public vendor-input
capture recipe refuses these interfaces. This is a collector limitation, not proof
that the device lacks inputs. Share the default numeric bundle first; include raw
descriptors only after privacy review and explicit permission.

An access failure is evidence. Keep it and the relevant numeric interface details.
Do not rerun as root, chmod hidraw nodes, disable SELinux or alter udev rules as an
automatic research step. A supported setup fix belongs to the setup workflow and
needs the person's agreement; an unmodified failure remains useful to triage.

## Compare observations, not temporary IDs

For a requested wired/receiver/Bluetooth or Windows/Fedora comparison, collect a
new local dossier for each condition. Record the person's connection mode and the
exact Neuron version beside each reviewed issue draft. Export the intended device
from each dossier separately; do not merge private files or edit verified bundles.

Match model, VID/PID, observed revision, top-level usage and report lengths, with
the person's confirmation of the physical unit. Dossier IDs and OS topology keys
are not stable across runs. Windows parsed fields and Linux raw descriptors are
different views, so their fingerprints need not match. Receiver groupings are not
proof of per-device command routing or pairing.

Report the earliest observed difference: enumeration, metadata, access, recognized
route, rendered output or physically confirmed outcome. Keep later untested steps
unknown. A cross-platform discrepancy is a research lead, not proof that the device
or operating system is defective.
