# ADR-0015: Agent OS Cross-Entity Primitives

- Status: Accepted
- Date: 2026-02-28
- Deciders: Temper core maintainers
- Related:
  - ADR-0004: Cedar Authorization for Agents
  - ADR-0013: Evolution Loop Agent Integration
  - `.vision/agent-os.md` (Agent OS vision)
  - `crates/temper-server/src/state/dispatch.rs` (action dispatch pipeline)
  - `crates/temper-authz/src/engine.rs` (Cedar evaluation)

## Context

Temper's Agent OS vision requires agents to generate IOA specs as operational plans,
delegate to sub-agents, and coordinate through shared verified state. Three capabilities
are missing from the current runtime:

1. **Cross-entity state gates** — transitions cannot block on another entity's state at runtime.
   A LeadAgent cannot gate its `Promote` action on `TestWorkflow.status == "Passed"`.

2. **Entity spawning from actions** — transitions cannot create child entities.
   A LeadAgent cannot spawn a `TestWorkflow` as part of `StartPlan`.

3. **Entity state in Cedar context** — authorization cannot reference another entity's state.
   A Cedar policy cannot gate `DeployWorkflow.canary_deploy` on `LeadAgent.status == "canary_ok"`.

All three share a common primitive: resolving another entity's current status at the dispatch layer.

## Decision

### Sub-Decision 1: Shared Entity Status Lookup (Foundation)

Add `resolve_entity_status()` to `entity_ops.rs` — fast path through `entity_state_cache`
(BTreeMap, already populated on every action dispatch), slow path via `get_tenant_entity_state()`.

Budget constants enforce bounded resource usage:
- `MAX_CROSS_ENTITY_LOOKUPS = 4` — per-transition cap on cross-entity queries
- `MAX_SPAWNS_PER_TRANSITION = 8` — per-transition cap on child entity spawns

**Why this approach**: The cache is already populated on every successful dispatch
(line 882 of dispatch.rs). Fast path avoids extra actor asks for the common case.
Budget constants follow TigerStyle bounded-resource principles.

### Sub-Decision 2: Cross-Entity State Gates

Design: Pre-resolve at dispatch layer, inject as booleans into `EvalContext`. Guards stay pure.

New `Guard::CrossEntityState` variant in the spec, mapped to `Guard::CrossEntityStateIn` in JIT.
At dispatch time, the current entity's fields provide the target entity ID; the target's status
is resolved via the shared lookup. Results are injected as `__xref:{type}:{field}` booleans
into `EvalContext`, keeping guard evaluation deterministic and free of I/O.

**Why this approach**: Guards must remain pure functions for verification (Stateright model
checking, deterministic simulation). Pre-resolving at the dispatch boundary maintains this
invariant while enabling cross-entity coordination.

### Sub-Decision 3: Entity Spawning from Actions

New `Effect::Spawn` variant in the spec, mapped to `Effect::SpawnEntity` in JIT.
`apply_effects()` collects spawn requests; the dispatch pipeline executes them
post-transition (same pattern as `ScheduledAction`). Child entities are created via
`get_or_create_tenant_entity()` with `parent_type`/`parent_id` in initial fields.

**Why this approach**: Spawn is an effect (post-state-change), not a guard (pre-state-change).
Executing spawns post-transition keeps the transition itself deterministic. The parent stores
the child ID via `store_id_in`, enabling cross-entity guard resolution on subsequent actions.

### Sub-Decision 4: Entity State in Cedar Context

New `[[context_entity]]` section in IOA specs declares entities whose status should be
available during authorization. At dispatch time (between entity state fetch and authz check
in `bindings.rs`), context entities are resolved and injected as `ctx_{name}_status` into
Cedar's context map.

**Why this approach**: Cedar policies need runtime state to make fine-grained decisions.
Injecting resolved state into the context record keeps Cedar evaluation pure — the engine
doesn't need to know about entity actors or async resolution.

## Rollout Plan

1. **Phase 0 (Immediate)** — All three primitives in a single PR. Unit tests for spec parsing,
   JIT guard/effect mapping, and integration tests for the full Agent OS scenario.

2. **Phase 1 (Follow-up)** — Evolution Engine support: unmet cross-entity guards surface as
   trajectory spans, enabling the sentinel to suggest spec changes.

## Consequences

### Positive
- Agents can generate multi-entity IOA specs as operational plans
- Parent-child entity relationships are first-class (spawn + cross-entity gates)
- Cedar policies can express rich cross-entity authorization rules
- All three primitives share a single resolution mechanism (low complexity)

### Negative
- Cross-entity lookups add latency to action dispatch (mitigated by cache fast path)
- Spawn chains could create many entities (mitigated by MAX_SPAWNS_PER_TRANSITION budget)

### Risks
- Cache staleness: entity_state_cache may have stale status between writes.
  Mitigation: slow path falls back to actor ask, which is always fresh.
- Circular spawn chains: entity A spawns B which spawns A.
  Mitigation: budget enforcement caps total spawns per transition.

### DST Compliance
- All new maps use `BTreeMap` (not `HashMap`) for deterministic iteration
- Entity ID generation uses `sim_uuid()` (not random)
- No new thread spawning — spawn dispatch is async in existing pipeline
- Guards remain pure — cross-entity state pre-resolved as booleans
- Simulation handler passes empty cross-entity map (deterministic isolation)

## Non-Goals

- Cross-entity invariant verification in Stateright (requires multi-entity model; deferred)
- Real-time subscription to entity state changes (event-driven cross-entity; deferred)
- Cross-tenant entity references (security boundary; out of scope)

## Alternatives Considered

1. **Lazy guard resolution inside actor** — Guards would call async lookups during evaluation.
   Rejected: breaks deterministic simulation and makes guards impure.

2. **Event-driven cross-entity coordination** — Entities subscribe to state changes.
   Rejected: adds pub/sub complexity; pre-resolution is simpler and sufficient for MVP.

3. **Inline Cedar entity lookups** — Cedar engine fetches entity state during evaluation.
   Rejected: Cedar evaluation must be synchronous and pure; pre-resolution at dispatch is cleaner.
