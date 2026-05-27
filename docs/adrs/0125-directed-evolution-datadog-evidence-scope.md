# ADR-0125: Directed Evolution Datadog Evidence Scope

- Status: Proposed
- Date: 2026-05-27
- Deciders: Temper core maintainers
- Related:
  - ADR-0120: Directed Evolution Control Plane
  - ADR-0121: Directed Evolution Runtime Refs In Evaluation

## Context

Directed Evolution signals can come from Datadog monitors, logs, traces,
metrics, or from a brain reading observability. The current `signal_observer`
prompt records the raw signal summary and evidence artifact id, but it does
not explicitly require the observer brain to verify Datadog-backed signals
against Datadog itself.

That makes Mission Control evidence weaker than the product contract: a
suggested direction may be based on a Datadog-looking summary without a
structured record of the actual Datadog surface, query, result, or URL.

## Decision

The Directed Evolution `signal_observer` prompt will require observer brains
to return a structured `evidence_scope` array. For Datadog-originated signals,
the prompt tells the brain to inspect Datadog logs, traces, metrics, monitors,
or dashboards and to avoid treating the signal summary alone as proof.

The `WorkItem.CorrelationJson` created for observer runs will preserve the raw
signal `CorrelationJson` as `signal_correlation_json`, alongside the stable
signal, organism, source, kind, and evidence artifact ids.

## Consequences

- Observer WorkItems carry enough context for a background Codex brain to use
  Datadog deliberately.
- Direction provenance can include concrete Datadog evidence scope from the
  observer result.
- The change is hot-loadable as a Directed Evolution app WASM/spec bundle; it
  does not require a Railway deploy.

## Non-Goals

- This ADR does not add a direct Datadog client to the WASM module.
- This ADR does not require every non-Datadog signal to include a Datadog URL.
