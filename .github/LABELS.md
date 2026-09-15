# Labels

Three tiers, hard-capped. Exactly one Tier-1 label and exactly one Tier-2 label
per issue; Tier-3 labels are zero or more. A script can validate this; an agent
should.


This file is the reasoning. [`labels.yml`](labels.yml) is the configuration — the
same set as data, with colours. 


## Tier 1 — type (exactly one)

| label | meaning |
|---|---|
| `bug` | Existing behavior is broken. |
| `enhancement` | New behavior. |
| `rework` | Existing behavior rebuilt or rebalanced — same spec, better implementation. Includes refactor, rebalance, and re-derivation after new understanding. Not a catch-all for "I feel like rewriting it." |
| `documentation` | Docs only. |

## Tier 2 — area (exactly one)

| label | meaning |
|---|---|
| `area:device` | Device control: dpi, polling, brightness, battery, side-plate, scroll. |
| `area:lighting` | Lighting engine, effects, and presets. |
| `area:macros` | Macros, beacons, and the python bridge. |
| `area:audio` | Audio control: mute, gain, output flip. |
| `area:host` | `neuron-host` protocol hub: the OpenRGB / Chroma bridge. |
| `area:app-ui` | GUI / app rendering and UX. |
| `area:cli` | `neuron-cli` front-end. |
| `area:core` | `neuron-core` engine; repo-wide concerns that fit no narrower area. |
| `area:cross-platform` | Linux / Mac port and platform seams. |

## Tier 3 — execution flags (zero or more)

| label | meaning |
|---|---|
| `polish` | Small, batchable refinements. Group into one PR when convenient. |
| `straightforward` | Expected to be clear-cut: low ambiguity, bounded surface, independently verifiable. Would route to a lighter  / efficiency model  in an agentic framework. Replaces GitHub's "good first issue" framing — the signal is task shape, not newcomer-friendliness. |
| `delicate` | Fine-grained work where the details carry the whole result: visual proportion, copy tone, interaction feel. Correctness alone is not success; needs taste and iteration. Routes to stronger tiers. The opposite of `straightforward`. |
| `freetime capable` | Opt-in for bounded Free Time proposals. Never authorizes merge. |
| `risky` | Touches HID interception, device write paths, or the python bridge. An agent must stop and ask the owner before implementing; never picked up autonomously. |
| `autonomously-derived` | The issue originated in an autonomous session (Free Time, a heartbeat/cron job, unattended run), not from a developer's direct request or interactive work. Provenance of *creation*, not evidence quality. |
| `quality-of-life` | Low priority but nice: tightens an existing loop without changing guarantees. |

Provenance of *evidence* is not a label. Real-usage evidence belongs in the issue
body — the agent-ready template already mandates it — and a label that should be on
every well-formed issue distinguishes nothing.

Provenance of *session origin* is, and that is what `autonomously-derived` marks: it
tells the owner which issues were born while nobody was watching, which changes how
much to trust the framing before reading it.

Priority is not a label either. If it ever matters it becomes its own explicit tier
when the need is real, not a smuggled axis with one value.

###
## Outside the tiers (orthogonal, optional)

| label | meaning |
|---|---|
| `epic` | A large, multi-part effort with sub-issues. A size marker, never a type or area — an epic still carries its own Tier-1 and Tier-2 labels. |
| `needs-triage` | Intake state for reported-but-unconfirmed issues; cleared by triage, not by the taxonomy. |

## Lifecycle plumbing (not taxonomy)

`duplicate`, `invalid`, `question`, `wontfix`, `help wanted` — GitHub mechanics,
not part of the system above.

## Rules

1. Every triaged issue carries exactly one Tier-1 and exactly one Tier-2 label. The one
   exception is intake: an issue still marked `needs-triage` may be missing its Tier-2, because
   the bug template can't know the area a reporter hit. Triage adds the area and removes
   `needs-triage` in the same pass, and an agent filing an issue itself has no such excuse.
2. Tier-3 flags are optional and stack.
3. `risky` overrides `freetime capable`: if both apply, the agent stops and asks.
4. `straightforward` and `delicate` are mutually exclusive: they are opposite ends
   of one axis. An issue carries at most one.
5. New labels are added only when an existing label cannot express the
   distinction without stretching. Tier counts stay capped (4 / 9 / 7); the
   nine areas are the one deliberate exception.
6. Agents creating issues validate tiers before creating; agents picking work
   filter on Tier 2 + Tier 3 (`freetime capable` and not `risky`) before
   proposing.

## Applying it

**It applies itself.** Push a change to `labels.yml` on `main` and the `sync-labels` workflow
pushes it onto the repo within seconds. You only need the script by hand to preview a
change, or to sync from a branch:

```bash
bash .github/sync-labels.sh --dry-run   # show what would be pushed
bash .github/sync-labels.sh             # push labels.yml onto the repo now
```

The sync is idempotent and **never deletes**. A label added by hand survives the run
and is reported at the end as "not in labels.yml" — which is the prompt to either
write it down or remove it deliberately. Deleting a label silently un-triages every
issue carrying it, so that stays a decision, not a side effect.
