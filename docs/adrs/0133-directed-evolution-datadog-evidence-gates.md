# ADR-0133: Directed Evolution Datadog Evidence Gates

- Status: Proposed
- Date: 2026-05-27
- Deciders: Temper core maintainers
- Related:
  - ADR-0120: Directed Evolution Control Plane
  - ADR-0125: Directed Evolution Datadog Evidence Scope
  - `os-apps/directed-evolution/wasm/work_item_result_router`

## Context

Directed Evolution evaluation stages can declare `datadog_evidence_scope` in
`RequiredEvidenceJson`. Today that requirement is advisory: the evaluator brain
may still return `passed: true` with only runtime-local evidence, and the app
state machine will pass the stage.

That is too weak for Directed Evolution. Mission Control should not show a
stage as passed when the negotiated evaluation contract required Datadog
evidence and the brain did not provide an inspectable Datadog logs, traces, or
metrics link.

## Decision

The Directed Evolution app-owned result router will enforce Datadog evidence as
a first-class evaluation gate.

For reviewer and simulated-user `StageResult` WorkItems:

- If the linked `EvaluationStage.RequiredEvidenceJson` includes
  `datadog_evidence_scope`, the router requires the Codex output to include at
  least one `evidence_scope` item with an accepted Datadog URL.
- If the brain output claims the stage passed but lacks that Datadog evidence,
  the router records a failed/eliminated stage result with a clear failure
  reason instead of passing it.
- If Datadog evidence is not required for that stage, existing pass/fail
  semantics remain unchanged.

The evaluation prompt will also tell the brain that Datadog evidence is
mandatory when `RequiredEvidence` includes `datadog_evidence_scope`, and that it
must fail honestly if Datadog cannot be queried.

## Rollout Plan

1. Ship the router and prompt change in the Directed Evolution app bundle.
2. Hot-load the updated app ref into the control tenant through Genesis.
3. Re-run a growth episode with Datadog MCP enabled for the worker and verify
   that passed stages carry Datadog evidence URLs.

## Consequences

### Positive

- A passed stage means it satisfied the evidence contract, not only the brain's
  narrative.
- Mission Control can trust Datadog-backed stages without adding UI-only
  interpretation.
- Failed Datadog access becomes visible as a real elimination reason.

### Negative

- Evaluation stages that require Datadog will fail until the worker brain has
  access to Datadog tooling and returns a structured evidence scope.

### DST Compliance

- This change is confined to app WASM result routing. It does not introduce
  wall-clock time, randomness, threads, filesystem, or network behavior beyond
  the existing host-mediated OData calls already used by the router.

## Non-Goals

- This ADR does not add a Datadog client inside app WASM.
- This ADR does not make every evaluation stage require Datadog.
- This ADR does not replace runtime OData and simulated-user evidence.

## Alternatives Considered

1. **Keep Datadog advisory** — Rejected because it allowed `datadog_available:
   0` stages to pass.
2. **Have Mission Control mark weak evidence visually** — Rejected because the
   state machine, not the UI, must decide whether an evaluation contract passed.
