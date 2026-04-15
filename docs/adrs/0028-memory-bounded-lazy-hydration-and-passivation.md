# ADR-0028: Memory-Bounded Entity Runtime with Lazy Hydration and Passivation

- Status: Proposed
- Date: 2026-03-12
- Deciders: Temper core maintainers
- Related:
  - ADR-0025: Evolution records as entities
  - `crates/temper-cli/src/serve/bootstrap.rs`
  - `crates/temper-server/src/state/entity_ops.rs`
  - `crates/temper-server/src/entity_actor/actor.rs`
  - `crates/temper-server/src/entity_actor/types.rs`
  - `crates/temper-server/src/odata/read.rs`
  - `crates/temper-server/src/state/mod.rs`
  - `crates/temper-server/src/state/metrics.rs`
  - `crates/temper-server/src/state/policy_suggestions.rs`
  - `crates/temper-evolution/src/store.rs`

## Context

Server memory can grow unbounded during startup and normal operation because of eager hydration, full in-memory event retention per actor, full collection materialization in OData reads, and unbounded in-memory maps. This causes OOM failures in constrained environments.

## Decision

Adopt a bounded-memory runtime strategy in five parts: lazy hydration, snapshot-first replay, pre-materialization pagination/capping, bounded in-memory caches/maps, and idle actor passivation.

### Sub-Decision 1: Lazy Hydration by Default

At startup, populate only `(tenant, entity_type, entity_id)` index entries from persistence. Do not spawn actors during boot unless explicitly requested by `TEMPER_EAGER_HYDRATE=true`.

**Why this approach**: It removes startup-time N-entity actor allocation while keeping list/discovery semantics intact.

### Sub-Decision 2: Snapshot-First Replay with Bounded Recent History

On actor startup, load snapshot first and replay only deltas. Keep a bounded in-memory recent-events deque for observability while tracking total event count separately for budget enforcement.

**Why this approach**: It preserves event-sourced correctness, reduces replay costs, and avoids retaining full journals in RAM.

### Sub-Decision 3: OData Pre-Materialization Pagination and Caps

For queries without `$filter` and `$orderby`, apply `$skip`/`$top` at the ID level before state fetch. Add default page size and hard cap on entities materialized per request.

**Why this approach**: It prevents full collection hydration for large sets where ordering/filtering does not require full materialization.

### Sub-Decision 4: Global Map Budgets

Apply explicit budgets and deterministic eviction to mutable in-memory maps (`entity_state_cache`, `agent_hints`, metrics maps, policy suggestion maps, and deprecated in-memory record store maps).

**Why this approach**: It prevents long-lived process growth from background operational accumulation.

### Sub-Decision 5: Idle Actor Passivation

Track actor last-access timestamps and periodically stop/snapshot actors idle beyond a configurable timeout. Keep entity index entries so discovery and lazy reactivation continue to work.

**Why this approach**: It recovers memory from inactive entities without sacrificing correctness or addressing semantics.

## Rollout Plan

1. **Phase 0 (Immediate)** — ship lazy hydration default + env rollback.
2. **Phase 1 (Follow-up)** — ship snapshot-first replay and bounded recent event deque.
3. **Phase 2** — ship OData pre-materialization pagination/caps.
4. **Phase 3** — ship global map budgets and deterministic eviction.
5. **Phase 4** — ship passivation worker and idle timeout controls.

## Readiness Gates

- Existing tests pass for affected crates.
- New tests cover snapshot replay parity, pagination defaults/caps, and passivation/reactivation.
- No unbounded map/event growth on long-running smoke test.

## Consequences

### Positive
- Startup and steady-state memory usage become bounded and tunable.
- Large tenant/entity sets are safer for constrained deployments.

### Negative
- Additional eviction/passivation logic increases operational complexity.
- In-memory history is now partial (recent window), not full journal.

### Risks
- Aggressive budgets can increase cache misses and rehydration frequency.
- Passivation timing bugs could increase latency on first request after idle.

### DST Compliance

- Uses deterministic structures (`BTreeMap`, `VecDeque`) and `sim_now()` timestamps.
- Background passivation loop is explicitly non-simulation critical and annotated with `// determinism-ok`.

## Non-Goals

- Changing verification cascade semantics.
- Changing persistent event schemas.
- Replacing the deprecated in-memory record store architecture.

## Alternatives Considered

1. **Keep eager hydration and increase memory limits** — rejected; masks root causes and fails in constrained platforms.
2. **Full preloading with compressed state blobs** — rejected; still scales with entity count at boot.

## Rollback Policy

Set `TEMPER_EAGER_HYDRATE=true` to restore eager boot behavior while retaining other mitigations. Individual budgets can be raised by env vars if needed during incident response.
