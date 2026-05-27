# ADR-0120: Directed Evolution Control Plane

- Status: Proposed
- Date: 2026-05-26
- Deciders: Temper core maintainers
- Related:
  - RFC-0001: Directed Evolution
  - ADR-0013: Evolution Loop Agent Integration
  - ADR-0025: Evolution Records & Governance Decisions as System Entities
  - ADR-0034: GEPA-Based Self-Improvement Loop
  - ADR-0035: IntentDiscovery Evolution Loop
  - `os-apps/evolution/`
  - `os-apps/intent-discovery/`

## Context

Temper already has several pieces of an evolution system: trajectory capture,
O/P/A/D/I records, `IntentDiscovery`, and a GEPA-oriented `EvolutionRun`.
Those pieces do not yet express the product contract in RFC-0001:

- a running app as the organism being evolved
- brain-observed signals becoming directions
- human-directed growth episodes
- repair episodes that can proceed automatically when bounded
- multiple real app variants per generation
- evaluation stages, metrics, evidence, eliminations, and selection
- AI simulated users as real agents rather than deterministic scripts
- promotion into a new organism parent version
- lineage visible across episodes
- Mission Control backed by live entity state

The existing `EvolutionRun` is useful but too narrow: it is optimized for
GEPA/spec mutation and does not model app variants, trials, lineage, explicit
selection evidence, or UI-facing direction provenance.

## Decision

Introduce a Temper-native Directed Evolution control plane. The control plane
is an OS app whose entity state is the source of truth for Directed Evolution
episodes. It may reuse implementation patterns from `os-apps/evolution`, but
its user-facing model follows RFC-0001 terminology.

### Sub-Decision 1: First-Class Evolution Entities

The control plane defines or materializes the following entities:

- `Organism`
- `OrganismVersion`
- `LineageEdge`
- `Signal`
- `Pressure`
- `Direction`
- `Episode`
- `Generation`
- `Variant`
- `Mutation`
- `AdaptationGoal`
- `ViabilityConstraint`
- `SelectionPressure`
- `EvaluationStage`
- `StageResult`
- `MetricDefinition`
- `Measurement`
- `EliminationRule`
- `ScoringRule`
- `EvidenceArtifact`
- `Trial`
- `Promotion`
- `AutonomyPolicy`
- `BrainRun`
- `WorkItem`

Not every entity needs a complex lifecycle in v1. Some are durable records with
simple `Draft -> Active -> Archived` or `Pending -> Recorded` shapes. The key
requirement is that Mission Control and background workers can understand the
entire episode by reading entity state and evidence links.

**Why this approach**: It preserves the Temper dogfooding rule. The evolution
state machine is visible, governable, queryable, and replayable instead of
being hidden in a worker process or UI store.

### Sub-Decision 2: Work Items Are The Execution Boundary

The control plane does not run Codex directly. When a brain or external action
is needed, an entity transition creates a `WorkItem` with:

- role (`observer`, `direction_framer`, `variant_generator`,
  `simulated_user`, `reviewer`, `selector`, `narrator`)
- target entity references
- bounded prompt/context references
- required output schema
- required evidence fields
- autonomy lane and policy references
- observability correlation tags

A TemperPaw worker claims runnable `WorkItem` entities, runs the appropriate
Codex job locally, and records results back through OData/entity actions.

**Why this approach**: Deployed Temper or Genesis does not need to host Codex.
The state machine remains deployed and inspectable, while local workers provide
the agent execution plane.

### Sub-Decision 3: Explicit Evaluation Language

The control plane uses the RFC-0001 evaluation vocabulary:

- `EvaluationStage`, not assay
- `StageResult`
- `MetricDefinition`
- `Measurement`
- `EliminationRule`
- `ScoringRule`
- `EvidenceArtifact`

Variant-generation brains may not modify the evaluators, selection pressure,
rules, or viability constraints for their own variants. Changes to evaluation
artifacts must come from the human-facing director brain, an evaluation
designer brain, or an approved control-plane action before the generation runs.

**Why this approach**: Evaluation has to be trusted by the human. A candidate
that can move the goalposts can make selection meaningless.

### Sub-Decision 4: Autonomy Policy Is Visible State

`AutonomyPolicy` records which pressure classes can start and promote
automatically:

- bounded repair may auto-start and auto-promote
- supervised repair may auto-start but pause before promotion
- directed growth requires human approval to pursue
- UX, policy, and risky data changes require human approval by default

Every `Direction`, `Episode`, and `Promotion` references the autonomy lane that
allowed or blocked it.

**Why this approach**: The human must be able to see what the system is allowed
to do without asking. Automation is a product surface, not only a policy file.

### Sub-Decision 5: Correlation Tags Are Required

All entities, worker submissions, trials, and evidence artifacts carry stable
correlation fields where applicable:

- `organism_id`
- `organism_version_id`
- `direction_id`
- `episode_id`
- `generation_id`
- `variant_id`
- `trial_id`
- `brain_run_id`
- `work_item_id`
- `simulated_user_id`
- `tenant`
- `app_ref`
- `environment`

**Why this approach**: Datadog traces/logs/metrics are evidence. Without stable
join keys, Mission Control cannot truthfully connect UI state to observability.

## Rollout Plan

1. **Phase 0** - Ship this ADR and RFC-0001 in the implementation branch.
2. **Phase 1** - Add the Directed Evolution OS app specs, CSDL, Cedar policy,
   and seed data needed for Agent Answers.
3. **Phase 2** - Add OData-accessible actions for direction creation, episode
   start, work item claiming, result recording, elimination, selection, and
   promotion.
4. **Phase 3** - Wire TemperPaw worker execution against `WorkItem`.
5. **Phase 4** - Wire Genesis Mission Control to live entities.
6. **Phase 5** - Prove one complete Agent Answers episode end to end before
   merging or deploying.

## Readiness Gates

- The OS app installs locally.
- Entity specs pass the verification cascade.
- A fixture-free local run creates an organism, direction, episode, generation,
  variants, stage results, elimination, selection, promotion, and lineage.
- Every worker-created result references a `BrainRun` and `WorkItem`.
- Every stage result has evidence or an explicit failure explaining why
  evidence could not be collected.
- Mission Control can render the episode from entities alone.

## Consequences

### Positive

- Directed Evolution becomes inspectable as Temper state.
- Mission Control can be live and truthful instead of fixture-driven.
- Local Codex workers can execute without moving the control plane out of
  Temper/Genesis.
- Existing IntentDiscovery and GEPA work can be incorporated instead of
  discarded.

### Negative

- The entity model is larger than a single `EvolutionRun`.
- The first vertical slice must implement enough records to be legible before
  the full engine is complete.
- Worker and UI teams must agree on output schemas and correlation fields.

### Risks

- Too many tiny entities could make the UI and OData flows noisy. Mitigation:
  keep simple records simple and expose episode-oriented query helpers only
  after the canonical entities are in place.
- The old `EvolutionRun` and new `Episode` language can confuse contributors.
  Mitigation: RFC-0001 is the naming source for user-facing Directed Evolution.

### DST Compliance

- Specs and transition tables remain deterministic.
- Simulation-visible crates must keep using `sim_now()`, `sim_uuid()`, bounded
  collections, and existing determinism rules.
- Worker execution is outside simulation-visible runtime paths and records
  results back through explicit entity actions.

## Non-Goals

- This ADR does not deploy Codex inside Railway or Genesis.
- This ADR does not replace existing trajectory or evolution-record storage.
- This ADR does not create an in-app chat assistant for Mission Control.
- This ADR does not allow humans to manually override the selected winner.

## Alternatives Considered

1. **Extend only `EvolutionRun`** - Rejected for v1 because the current entity
   is too GEPA/spec-mutation specific and does not model app variants, trials,
   and lineage clearly.
2. **Put orchestration in TemperPaw Rust code** - Rejected because it would
   hide core product state outside Temper entities.
3. **Build Mission Control first with fixtures** - Rejected because the product
   requirement is a truthful live UI.

## Rollback Policy

The Directed Evolution OS app is additive. It can be uninstalled or hidden
without removing existing `evolution` or `intent-discovery` apps. If the model
proves too broad, keep the recorded RFC/ADR and split the entities into smaller
apps without changing the external work item contract.
