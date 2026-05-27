# ADR 0129: Directed Evolution Repair-Aware Variant Lanes

## Status

Accepted

## Context

A live repair-autostart cycle for Agent Answers reached generation and
evaluation, but no variant survived. One cause was prompt drift: the episode was
classified as bounded repair, yet the generated variant prompts still used
growth-flavored lane suggestions such as intent capture, evidence fields, and
answer usefulness. Evaluators correctly eliminated variants that expanded the
product surface instead of repairing the observed acceptance-visibility failure.

Repair episodes need diversity, but that diversity must stay inside the repair
boundary.

## Decision

Directed Evolution episode orchestration now chooses repair-specific variant
lanes when the episode context indicates repair pressure. The repair lanes focus
on:

- the exact failing lifecycle path
- automatically maintained visibility/index repair
- executable regression coverage plus minimal spec/CSDL/Cedar updates

The generic growth lanes remain available for non-repair episodes. Repair
prompts also explicitly tell variant generators not to add product-growth
features, intent-capture affordances, or optional metadata unless required by
the repair and automatically maintained by the existing lifecycle.

## Consequences

- Repair variants should be less likely to fail because they solved a different
  product-growth problem.
- Mission Control will show elimination reasons that line up with the repair
  contract instead of avoidable prompt mismatch.
- This is hot-loadable app/WASM behavior and does not require a Railway deploy.

## Non-Goals

- This does not change the number of generated variants.
- This does not modify evaluators or selection rules.
- This does not weaken viability constraints for repair episodes.
