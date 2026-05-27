# ADR-0126: Directed Evolution Organism Parent Ref Sync

- Status: Proposed
- Date: 2026-05-27
- Deciders: Temper core maintainers
- Related:
  - ADR-0120: Directed Evolution Control Plane
  - ADR-0124: Directed Evolution Promotion Materialization

## Context

Directed Evolution promotes a winning variant by creating a new
`OrganismVersion`, superseding the old parent, and recording that version on the
top-level `Organism`. The live Agent Answers control tenant proved a mismatch:
`Organism.OrganismVersionId` pointed at the promoted `da94ba2...` version while
`Organism.AppRef` still pointed at the original baseline `d519703...` app.

Mission Control and subsequent evolution prompts need the top-level organism to
truthfully describe the current parent. A lineage graph that shows the new
version but a stale organism app ref is misleading, and future episodes may
start from the right `OrganismVersion` while presenting the wrong app reference.

## Decision

`Organism.RecordOrganismVersion` will carry the promoted `AppRef` alongside the
new `OrganismVersionId`, `PromotionId`, and `Summary`. The selector router will
pass the winning variant's app ref into that action when it records a promoted
version.

Add a separate idempotent `Organism.SyncOrganismParentRef` action for live
repairs and materialization reconciliation. It updates the organism's current
parent fields without incrementing `version_count`. The Directed Evolution
Cedar policy will authorize that action for the same trusted organism mutation
principals that can record organism versions.

## Rollout Plan

1. **Phase 0** — Update Directed Evolution specs, CSDL, selector WASM, and spine
   tests. Hot-load the updated app through Genesis.
2. **Phase 1** — Dispatch `SyncOrganismParentRef` in the control tenant to repair
   the existing Agent Answers organism row.
3. **Phase 2** — Run the next human-directed growth episode from the repaired
   parent state.

## Readiness Gates

- The directed-evolution WASM spine test proves `Organism.AppRef` follows the
  selected winner.
- The spine test proves `SyncOrganismParentRef` updates parent summary fields
  without incrementing `version_count`.
- The hot-loaded control tenant accepts `SyncOrganismParentRef`.
- The live Agent Answers organism shows matching `AppRef` and current
  `OrganismVersion.AppRef`.

## Consequences

### Positive

- Mission Control can trust the top-level organism row as the current parent
  summary.
- Subsequent episode prompts and lineage displays no longer disagree about the
  app ref being evolved.
- Existing stale organisms can be repaired without fake version increments.

### Negative

- `RecordOrganismVersion` gains one more action parameter.
- Older app installs that do not include this action parameter can still have
  stale organism app refs until hot-loaded.

### DST Compliance

This change is limited to app specs and WASM integrations. It does not touch
simulation-visible Rust crates.

## Non-Goals

- This ADR does not change promotion winner selection.
- This ADR does not deploy Railway.
- This ADR does not replace canonical promotion materialization.

## Alternatives Considered

1. **Repair with another `RecordOrganismVersion` call** — Rejected because that
   would increment `version_count` for a metadata sync.
2. **Only read `OrganismVersion.AppRef` in Mission Control** — Rejected because
   it leaves the organism entity itself internally inconsistent.

## Rollback Policy

Hot-load the previous Directed Evolution app version if the new action contract
misbehaves. Existing organism state can be corrected manually by hot-loading the
fixed app again and dispatching `SyncOrganismParentRef`.
