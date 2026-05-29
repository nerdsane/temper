# ADR-0124: Directed Evolution Proof Telemetry And Diffs

- Status: Accepted
- Date: 2026-05-29
- Deciders: Temper core maintainers
- Related:
  - ADR-0120: Directed Evolution control plane
  - ADR-0121: Directed Evolution runtime refs in evaluation
  - ADR-0122: Genesis pinned app install
  - ADR-0123: Hot-load CSDL action preservation
  - `os-apps/directed-evolution`
  - `crates/temper-server/src/runtime_metrics.rs`
  - `crates/temper-server/src/odata`

## Context

Directed Evolution had enough state-machine shape to describe episodes, variants, and evaluation stages, but the proof loop still had review gaps. Reviewers could not see concrete app diffs, Datadog evidence was not a hard judging surface, simulated-user output could blur into pass/fail decisions, and promotion/lineage records did not consistently preserve the winning mutation story.

The implementation also needed a clean boundary between semantic entities authored by the human/Codex director and mechanical work performed by WASM/workers. Cross-entity fan-out should be declared through Temper reactions and workflow routing, while workers and WASM perform runtime I/O, deterministic checks, Codex execution, Datadog queries, and promotion materialization.

## Decision

Directed Evolution will store first-class proof data for every end-to-end episode:

1. Episodes are started from an already-authored semantic protocol graph: `Episode`, `AdaptationGoal`, `ViabilityConstraint`, `MetricDefinition`, `EvaluationStage`, `SimulatedUserPlan`, and `SelectionProtocol`.
2. Variant generation records `ChangedFilesJson` and `DiffPatch` on `Variant`, `Mutation`, and promoted `LineageEdge` records.
3. Simulated-user trials record journeys and blockers only. They cannot directly pass or fail a variant.
4. Stage evaluators record explicit provenance: `agent-observed`, `brain-judged`, `state-verified`, `wasm-computed`, `runtime-measured`, or `datadog-measured`.
5. The state verifier is a mechanical evaluator that rejects missing mutation data, out-of-organism changes, evaluator-bundle mutations, and missing required changed files.
6. Datadog telemetry gates are mandatory when declared by the stage. They must query real Datadog logs scoped by episode, variant, and runtime tenant, and zero matching logs means failure.
7. Selection marks surviving non-winners as `NotSelected` with a readable selection-elimination reason instead of leaving them as apparently viable active branches.
8. Promotion records the winner, hot-load materialization, and a lineage edge containing the promoted diff.

## Rollout Plan

1. **Phase 0 (Immediate)** — Ship app specs, WASM router changes, OData runtime request telemetry, and proof fields for diffs/evidence.
2. **Phase 1 (Proof)** — Run a fresh Agent Answers episode with Codex-as-human, separate Codex worker roles, real simulated users, Datadog telemetry gates, selection, and promotion.
3. **Phase 2 (Follow-up)** — Add richer evaluator bundles and additional deterministic/WASM evaluators without allowing variants to modify the evaluator that judges them.

## Readiness Gates

- A fresh episode has real variants with non-empty app diffs.
- Simulated users exercise hot-loaded runtimes and only report observations.
- Datadog telemetry evaluators pass or fail from real Datadog query results.
- State-verifier results are stored with `state-verified` provenance.
- A selected winner is promoted and a lineage edge records the promoted diff.
- The losing survivor or failed branch has a readable death/selection-elimination report.

## Consequences

### Positive

- Reviewers can inspect what changed, why variants died, what evidence judged them, and what was promoted.
- Datadog links become secondary raw evidence behind stored query summaries and counts.
- The evaluator boundary is explicit and mechanically checked.

### Negative

- Episodes produce more entities and evidence rows.
- A full acceptance proof is slower because it runs real Codex worker roles and Datadog queries.

### Risks

- Datadog field indexing can drift. The telemetry evaluator mitigates this by querying episode, variant, and tenant fields and by storing the exact query and interpretation.
- Selection-eliminated variants are not stage failures; the UI must label them distinctly from hard evaluator eliminations.

### DST Compliance

- Runtime telemetry uses request metadata already available to the server and does not affect simulation scheduling.
- Directed Evolution WASM routes existing action outputs and does not introduce nondeterministic iteration in simulation-visible crates.

## Non-Goals

- This ADR does not define a general-purpose evaluator authoring language.
- This ADR does not migrate legacy proof tenants.
- This ADR does not make the UI the natural-language negotiation surface; chat remains the director/brain negotiation channel.

## Alternatives Considered

1. **Store diffs only in Git/Genesis** — Rejected because reviewers need diffs in the evolution episode and lineage context without leaving Mission Control.
2. **Let simulated users pass/fail variants** — Rejected because simulated users represent app usage, not evaluators.
3. **Treat missing Datadog telemetry as neutral** — Rejected for telemetry-gated stages because the organism must be judged from real runtime observability.

## Rollback Policy

Revert the Directed Evolution app version to the prior Genesis-published app ref and roll back the Temper commit that adds the proof fields, runtime telemetry, and router fan-out. Existing proof tenants remain historical records and do not require migration.
