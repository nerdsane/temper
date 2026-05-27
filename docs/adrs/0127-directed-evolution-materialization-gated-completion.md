# ADR-0127: Directed Evolution Materialization-Gated Completion

- Status: Proposed
- Date: 2026-05-27
- Deciders: Temper core maintainers
- Related:
  - ADR-0120: Directed Evolution Control Plane
  - ADR-0124: Directed Evolution Promotion Materialization
  - ADR-0126: Directed Evolution Organism Parent Ref Sync
  - `os-apps/directed-evolution/specs/episode.ioa.toml`
  - `os-apps/directed-evolution/wasm/work_item_result_router`

## Context

ADR-0124 made canonical promotion materialization explicit: after a selector
chooses a winner, a promoter `WorkItem` publishes the winning Genesis app ref
and hot-loads it into the production tenant. The selector currently records the
winner, queues the promoter work item, and immediately completes the episode.

That weakens Mission Control's truth contract. An episode can appear
`Completed` while the canonical publish/install is still pending, or even after
that materialization later fails. For Directed Evolution, a completed episode
must mean the organism has both selected a new parent and materialized that
parent into the runtime surface that future generations inherit from.

## Decision

Directed Evolution episode completion is gated on promotion materialization.

### Sub-Decision 1: Selector Stops At Promoting

The selector remains responsible for selection state:

- select the winning variant
- eliminate non-winning survivors
- create the `Promotion`, new `OrganismVersion`, and `LineageEdge`
- update the `Organism` parent ref
- queue the promoter `WorkItem`

It must not dispatch `Episode.CompleteEpisode`. After
`Episode.RecordEpisodeWinner`, the episode stays in `Promoting` until the
promoter result is routed.

**Why this approach**: Selection and materialization are distinct lifecycle
edges. Keeping `Promoting` visible lets Mission Control show a selected winner
whose canonical runtime install is still in flight.

### Sub-Decision 2: Promoter Success Completes The Episode

When the promoter result records `Promotion.RecordPromotionMaterialization`,
the result router also completes the owning episode if it is still
`Promoting`. The `CompleteEpisode` payload uses the existing `PromotionId`,
`NewOrganismVersionId`, and materialization summary.

**Why this approach**: The promoter is the first component with authoritative
evidence that the canonical Genesis publish/install happened.

### Sub-Decision 3: Promoter Failure Fails The Episode

When a promoter `WorkItem` fails after selection, the router records
`Promotion.RecordPromotionMaterializationFailure` and fails the episode if it
is still `Promoting`.

**Why this approach**: A failed materialization means the evolution cycle did
not reach the requested end state, even though selection succeeded.

## Rollout Plan

1. **Phase 0 (Immediate)** - Update the Directed Evolution result router and
   spine tests so selector completion is no longer accepted as proof of a full
   episode.
2. **Phase 1 (Hot-load)** - Rebuild and publish the Directed Evolution app
   bundle, then hot-load the pinned version into the control tenant.
3. **Phase 2 (Live Proof)** - Run or resume a full episode and verify Mission
   Control shows `Promoting` before materialization and `Completed` only after
   the promoter succeeds.

## Readiness Gates

- The selector queues a promoter work item while the episode remains
  `Promoting`.
- The promoter success path marks the promotion materialized and then completes
  the episode.
- The promoter failure path records materialization failure and fails the
  episode.
- Existing Directed Evolution spec and WASM spine tests pass.

## Consequences

### Positive

- Mission Control's completed state matches the real end-to-end pipeline.
- Failed canonical publish/install work becomes visible as an episode failure,
  not a post-completion footnote.
- Future generations can treat completed episodes as having a materialized
  parent runtime.

### Negative

- Episodes spend longer in `Promoting` while external Genesis side effects run.
- Tests and live runbooks must wait for promoter completion before declaring a
  full cycle done.

### Risks

- A promoter work item that is never claimed leaves the episode in `Promoting`.
  This is correct but requires Mission Control and operations to surface stale
  promoter work clearly.

### DST Compliance

- This change only updates Directed Evolution WASM integrations and OS app
  tests. It does not add wall-clock, random, threaded, or filesystem behavior
  to simulation-visible runtime crates.

## Non-Goals

- This ADR does not change how promoter side effects publish or hot-load
  Genesis apps.
- This ADR does not add human winner override or approval gates.
- This ADR does not redesign the promotion entity lifecycle.

## Alternatives Considered

1. **Keep completing in the selector** - Rejected because it lets the UI claim
   completion before production materialization exists.
2. **Add a new Episode state after Promoting** - Rejected for this slice
   because the existing `Promoting` state already captures the in-flight
   materialization boundary.

## Rollback Policy

Revert the router and test changes. Existing Directed Evolution apps can be
hot-loaded back to the previous pinned version if the new materialization gate
breaks live progress.
