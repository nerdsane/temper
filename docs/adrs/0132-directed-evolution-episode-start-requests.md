# ADR-0132: Directed Evolution Episode Start Requests

- Status: Proposed
- Date: 2026-05-27
- Deciders: Temper core maintainers
- Related:
  - ADR-0120: Directed Evolution Control Plane
  - ADR-0124: Directed Evolution Promotion Materialization
  - ADR-0128: Directed Evolution Policy-Gated Repair Autostart
  - ADR-0131: Directed Evolution Follow-Up Generations
  - `os-apps/directed-evolution`

## Context

Human-gated growth episodes start after a human and a brain negotiate the
Adaptation Goal, Viability Constraints, metrics, evaluation stages,
elimination rules, and scoring rules in chat. The first bridge for this was a
TemperPaw command that imperatively created each Directed Evolution entity and
then dispatched `Direction.SelectDirection` and `Episode.StartEpisode`.

That bridge moves the pipeline forward, but it hides too much orchestration in a
worker. TemperPaw's operating rule is that state-changing orchestration belongs
in Temper entities and WASM integrations. The external worker should submit the
already-negotiated contract as one governed event; Directed Evolution should own
materializing that contract into episode state.

## Decision

Add an `EpisodeStartRequest` entity to the Directed Evolution app. A request
records the negotiated contract and is submitted through a single
`SubmitEpisodeStartRequest` action. The action triggers a Directed Evolution
WASM module that:

1. creates the `Episode`;
2. begins episode negotiation with the chosen Direction, Organism, parent
   version, and autonomy lane;
3. activates MetricDefinitions, AdaptationGoal, ViabilityConstraints,
   EliminationRules, ScoringRules, SelectionPressure, and EvaluationStages;
4. records the episode contract;
5. selects the Direction for that Episode;
6. starts the Episode, which lets the existing `episode_orchestrator` create the
   first generation and variant work items; and
7. marks the request as started with the materialized `EpisodeId`.

The request is not the brain. It is the audited handoff from a brain/human
conversation into the reusable Directed Evolution engine.

**Why this approach**: It preserves the user's desired chat-first negotiation
while keeping Mission Control truthful. The UI can show a Direction, the
submitted contract, and the exact request that started the episode. TemperPaw
stays a thin client of the app instead of owning the episode graph.

## Rollout Plan

1. **Phase 0 (Immediate)** - Add the entity spec, CSDL, Cedar permission, WASM
   module, and local tests. Update the TemperPaw starter to submit one request
   instead of creating all child entities directly.
2. **Phase 1 (Hot-load)** - Rebuild, publish, and hot-load the Directed
   Evolution app bundle into the control tenant. Do not deploy Railway for this
   app-only change.
3. **Phase 2 (Live proof)** - Start a human-gated growth episode by submitting
   an `EpisodeStartRequest`, then observe generation, evaluation, elimination,
   selection, promotion, lineage, and Genesis hot-load evidence.

## Readiness Gates

- A submitted request creates and starts exactly one Episode.
- The Episode has a recorded contract before `StartEpisode`.
- The Direction moves to `Selected` with the materialized Episode id.
- The request stores the materialized `EpisodeId` for Mission Control.
- The external worker can start a human-gated episode with one create plus one
  action call.

## Consequences

### Positive

- Directed Evolution owns episode materialization as app behavior.
- Mission Control can inspect the human/brain handoff as first-class state.
- TemperPaw no longer needs to encode Directed Evolution's multi-entity setup.
- The path remains hot-loadable through Genesis pinned refs.

### Negative

- The Directed Evolution app gains one more entity and WASM module.
- The request contract has to be intentionally versioned as fields and JSON
  payloads evolve.

### Risks

- A malformed request could create partial child entities before failing.
  Mitigation: validate required contract inputs first and fail before side
  effects where possible.
- The request action can be retried. Mitigation: request state becomes
  `Started` after success, so the same request cannot be submitted again.

## Non-Goals

- This ADR does not put chat inside Mission Control.
- This ADR does not change repair autostart; repair can keep using its existing
  observer/autonomy path.
- This ADR does not deploy Codex or TemperPaw inside Railway.

## Alternatives Considered

1. **Keep the worker-owned bridge** - Rejected because it hides orchestration in
   imperative worker code and makes Mission Control less authoritative.
2. **Make the UI call many entity actions directly** - Rejected because the UI
   should not own the episode graph and the user expects negotiation to happen
   in chat.
3. **Start the Episode directly with expanded parameters** - Rejected because
   `Episode` should remain the lifecycle entity, not the full contract
   submission surface.

## Rollback Policy

Hot-load the previous Directed Evolution pinned ref if request materialization
misbehaves. Existing Episodes, Directions, and contract entities remain valid
because the request only adds a new entry path.
