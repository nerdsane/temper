# ADR-0124: Directed Evolution Promotion Materialization

- Status: Proposed
- Date: 2026-05-27
- Deciders: Temper core maintainers
- Related:
  - ADR-0120: Directed Evolution control plane
  - ADR-0121: Directed Evolution runtime refs in evaluation
  - ADR-0122: Genesis pinned app install
  - ADR-0123: Hot-load CSDL action preservation
  - `os-apps/directed-evolution`

## Context

The Directed Evolution selector can choose a winning variant, create a `Promotion`, mark the winning `Variant` promoted, create a new parent `OrganismVersion`, and record a lineage edge. During the live Agent Answers run, that internal evolutionary record succeeded, but the canonical Genesis app had to be advanced manually: push the winner commit to `refs/heads/main`, publish the new app version, and hot-load the pinned app into the production tenant.

That manual gap breaks the desired end-to-end pipeline. Promotion must mean both "this variant won the evolutionary selection" and "the selected organism version was materialized into the canonical runtime surface." Because Genesis publication and install are external side effects, the Directed Evolution WASM should not perform them directly. It should queue work and record the result as entity state.

## Decision

Directed Evolution promotions will gain an explicit materialization phase:

1. The selector continues to record the winner, `Promotion`, new `OrganismVersion`, and `LineageEdge`.
2. The selector also queues a `WorkItem` with role `promoter`, `TargetEntityType = Promotion`, and `TargetEntityId = <promotion_id>`.
3. The external worker materializes the promotion by pushing the winning app ref to the canonical Genesis ref, publishing the app version, and hot-loading it into the configured production tenant.
4. When the `promoter` WorkItem succeeds, the result router records a materialization evidence artifact and dispatches `Promotion.RecordPromotionMaterialization`.
5. When the `promoter` WorkItem fails, the result router records the failure on the already-promoted `Promotion` through `Promotion.RecordPromotionMaterializationFailure`.

Promotion remains selection-owned: humans direct goals and constraints, evaluators select the winner, and the promoter only materializes the already-selected winner.

## Rollout Plan

1. **Phase 0 (Immediate)** — Add Promotion materialization fields/actions, queue `promoter` WorkItems from selector success, and route promoter success/failure evidence.
2. **Phase 1 (Worker Pairing)** — Teach the TemperPaw Codex worker to execute `promoter` WorkItems as deterministic external materialization work rather than a Codex reasoning run.
3. **Phase 2 (Live Proof)** — Run a fresh Directed Evolution episode where canonical Genesis main/default promotion is completed automatically, not manually.

## Readiness Gates

- Selector success creates a queued `promoter` WorkItem for the new `Promotion`.
- Promoter success records canonical app ref, production tenant, runtime ref, summary, and evidence on the `Promotion`.
- Promoter failure is visible on the `Promotion` and linked to WorkItem evidence.
- A live proof shows Genesis main and the production tenant advancing from the worker, with no Railway deploy for the organism app.

## Consequences

### Positive

- Mission Control can distinguish selection from materialization.
- Promotion no longer depends on an operator performing Genesis steps by hand.
- The external side effects are still auditable as WorkItem and BrainRun state.

### Negative

- A `Promotion` can be evolutionarily promoted but not yet materialized if the worker is down or Genesis rejects the push/install.

### Risks

- The canonical push can fail if the winner commit does not fast-forward main. The worker should fail the promoter WorkItem rather than force-push.
- Re-running the exact same app hash into the same tenant can hit Genesis `AppInstallation` idempotency gaps. V1 treats already-materialized evidence as success only when the worker can prove the target ref and tenant already match.

### DST Compliance

- Directed Evolution app changes are IOA specs and WASM integrations. The WASM queues and records entity transitions only; it does not perform git, network publication, or wall-clock work.

## Non-Goals

- This ADR does not add human override of the selected winner.
- This ADR does not make variants modify evaluators or selection criteria.
- This ADR does not solve all Genesis AppInstallation idempotency behavior.

## Alternatives Considered

1. **Do Genesis publication inside selector WASM** — Rejected because it would hide external publication side effects in app logic and require git/network capabilities in WASM.
2. **Have the worker promote immediately inside selector role output handling** — Rejected because the Directed Evolution entity graph would not show the materialization step as first-class work.
3. **Leave canonical promotion manual** — Rejected because the goal is a fully working end-to-end pipeline.

## Rollback Policy

Disable or ignore `promoter` WorkItems and return to manual Genesis publication while keeping selector-owned evolutionary promotion intact.
