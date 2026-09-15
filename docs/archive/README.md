# archive

Dated snapshots. **Nothing in here is current, and nothing in here is authoritative.**

These are point-in-time research passes and diagnosis runs, kept because the reasoning
in them is worth having, not because the conclusions still hold. Each one states the
date it was taken. The live docs are one level up:

- [`../GDD.md`](../GDD.md) — what neuron does
- [`../TDD.md`](../TDD.md) — how it's built
- [`../PROTOCOL-HOST.md`](../PROTOCOL-HOST.md) — the protocol hub's design
- [`../STATUS.md`](../STATUS.md) — what actually works today

If you find something here that is still true and still matters, the fix is to move it
into one of those, not to cite this folder.

## what's in here

| file | what it was | why it's archived |
|---|---|---|
| `UX-PENTEST-2026-08-04.md` | A copy pass plus a click-through behavioural pen-test of the live GUI on real hardware. | A dated diagnosis run, not a design doc. **All of its findings are resolved** — the copy fixes at the time, findings #2-#5 in commit `f18442b`, and finding #1's symptom separately (its stated root cause turned out to be wrong). Kept for the harness notes, which are still the best description of how to pen-test this GUI. |
| `SYNAPSE-PARITY-2026-07-04.md` | Competitive research on what Synapse users love, hate, and miss. | Feature-gap research that drove the roadmap. Parts of it were already self-corrected ("this already exists in-tree") by the time it was written down, which is the tell that it's a snapshot rather than a map. |
