---
name: Agent-ready task
about: A bounded change with evidence, starting points, acceptance tests, and a clear handoff.
title: ""
labels: ""
assignees: ""
---

## Outcome
Describe the user-visible result, not merely an implementation activity.

## Evidence and current behavior
Separate reproduced observations, source inspection, and hypotheses. Include a minimal repro or relevant source links. Identify the inspected revision when useful; verify current source before editing.

Never include credentials, tokens, private user content, or machine-specific secrets.

## Scope and non-goals
State what this task may change and what it must leave alone. Reference implementations are examples, not mandatory dependencies or architectures.

## Starting points
List relevant files/symbols and existing tests or native capabilities. These are discovery leads, not a demand to preserve today's layout.

## Acceptance criteria
- [ ] A concrete, observable behavior.
- [ ] Relevant failure and recovery behavior.
- [ ] Privacy, consent, and accessibility requirements where applicable.
- [ ] No unrelated changes or undocumented regressions.

## Verification
Name the focused regressions and real user-facing path to exercise. For bug fixes, reproduce the failure before changing the implementation. Record actual commands/results and remaining uncertainty; do not substitute a mock-only pass for a browser, device, or runtime claim.

## Decisions left to the implementer
Identify genuine choices. Prefer the smallest native solution. Explain consequential tradeoffs; do not add dependencies or broaden permissions merely to match a reference implementation.

## Handoff
Return the change summary, verification output, known limitations, and reviewable diff/PR. Preserve unrelated working-tree changes. External publication and consequential account/configuration changes require the owner's approval.
