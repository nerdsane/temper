# ADR-0105: Data-Only Entity Create Fast Path

- Status: Proposed
- Date: 2026-05-19
- Deciders: Temper core maintainers
- Related:
  - ADR-0082: Projection Correctness Observability
  - ADR-0095: Projection Transaction Fast Path
  - ADR-0096: Set-Based Projection Index Reconciliation
  - ADR-0099: Local WASM TData Host Path
  - ADR-0104: Projection Read Parity And Local Tenant Propagation
  - `crates/temper-server/src/odata/write.rs`
  - `crates/temper-server/src/state/entity_ops.rs`

## Context

PERF-026 made TemperPaw's first-turn `SessionEntry` shape headerless. Production
Datadog evidence on `service.version=8c726f172e709f862b01b5b9c6ed5c1e9ccdd10d`
shows the provider-response apply envelope is now much lower, but the remaining
tail is still dominated by creating two `SessionEntry` rows:

- `Session.ProviderResponseReady.integrations`: average about `290.1 ms`
- `wasm:provider_response_applier`: average about `290.1 ms`
- `provider_response_applier` `append_session_tree`: average about `136.9 ms`
- accepted trace `11156646544924625715`: one SessionEntry `POST` costs about
  `109.7 ms`, the other about `77.9 ms`, and their `http_call_batch` overlap
  leaves `wasm.invoke.run` at about `145.0 ms`

The `SessionEntry` entity is spec-governed, durable, tenant-scoped, and part of
the conversation audit trail. It must not become an ad hoc side table. But its
IOA spec has exactly one state, `Recorded`, and no actions. Creating a
`SessionEntry` through generic OData currently pays the full actor path anyway:

1. resolve the entity type;
2. run write prechecks;
3. spawn a new entity actor;
4. have actor `pre_start` persist the bootstrap `Created` event;
5. ask the actor for `GetState`;
6. write the query projection;
7. return the OData entity.

That path is correct, but it does more orchestration than a no-transition entity
needs. The actor has no future transition to evaluate during creation, and the
initial state can be constructed from the transition table plus request fields.

## Decision

Add a generic data-only create fast path for entities whose `TransitionTable`
has no transition rules.

### Sub-Decision 1: Detect Eligibility From The Transition Table

An entity type is eligible only when its live transition table has an empty
`rules` list.

**Why this approach**: The gate is architectural, not entity-name based. It does
not hardcode `SessionEntry`, `Recorded`, or TemperPaw. A no-rule table means
there are no input/internal transitions, custom effects, scheduled actions,
spawns, or WASM triggers to run during create. Entities with any action keep the
existing actor path.

### Sub-Decision 2: Persist The Same Bootstrap Event

The fast path still writes a `Created` event to the configured event journal
with sequence `1`, the same action name, initial fields, timestamps, and actor
ID shape used by entity actors.

**Why this approach**: The event journal remains the source of truth. If the
entity is later hydrated through `get_tenant_entity_state`, normal actor replay
can recover the same state from the persisted event.

### Sub-Decision 3: Update The Query Projection Before Acknowledgement

The fast path calls the existing query projection writer before returning HTTP
`201`. Projection write failure remains a request failure.

**Why this approach**: This preserves the read-after-write contract restored by
ADR-0104. TemperPaw's `SessionEntry` helper keeps its session-scoped read-back
verification, so a `201` followed by an invisible row is still detected.

### Sub-Decision 4: Populate In-Memory Indexes Without Hydrating Actors

After the durable event and projection are written, the server updates the
entity index and emits the same observe/SSE creation event. It does not insert
an actor into the registry.

**Why this approach**: Collection reads and observe streams remain useful, while
the hot create path avoids actor spawn and `GetState`. Any later point lookup can
hydrate the actor from the event journal on demand.

### Sub-Decision 5: Keep OData, Cedar, Tenant, And Precheck Boundaries

The OData write handler still resolves the entity set, applies tenant
extraction, verification gates, and write prechecks before invoking the fast
path.

**Why this approach**: The fast path is only a replacement for unnecessary actor
hydration after the request has already passed the normal public contract.

## Rollout Plan

1. **Phase 0 (Immediate)** - Add the Temper server fast path behind the
   no-transition-rule eligibility check. Add focused tests using a no-action
   IOA entity and a normal action-bearing entity.
2. **Phase 1 (TemperPaw adoption)** - Merge Temper, bump TemperPaw's Temper git
   rev, deploy, and rerun the headerless `SessionEntry` live proof.
3. **Phase 2 (Production proof)** - Compare before/after Datadog windows for
   `Session.ProviderResponseReady.integrations`,
   `wasm:provider_response_applier`, `POST /tdata/SessionEntries`,
   `entity.get_or_create_tenant_entity`, and `append_session_tree`.

## Readiness Gates

- Focused Temper server tests prove no-rule entities create through the fast
  path, write projections, and replay correctly through actor hydration.
- Focused tests prove action-bearing entities keep the existing actor path.
- `cargo test -p temper-server` passes.
- DST review and code review pass before commit.
- TemperPaw dependency bump passes focused `SessionEntry` tests and full
  `cargo test --locked -p temperpaw`.
- Production proof shows no projection/read-back drift and a lower fixed-version
  provider-response apply tail.

## Consequences

### Positive

- Data-only entities avoid needless actor spawn and `GetState` on create.
- The optimization is reusable for any generated app entity that is immutable
  or append-only with no transitions.
- Event audit, projection correctness, tenant isolation, and OData semantics are
  preserved.

### Negative

- Entity creation gains a second internal path, so tests must prove both paths
  stay semantically equivalent.
- The fast path duplicates a small amount of initial-state/event construction
  that is currently private to `EntityActor`.

### Risks

- A spec with hidden side effects could be incorrectly classified as data-only.
  Mitigation: gate strictly on empty `TransitionTable.rules`, which is produced
  after IOA actions are translated.
- A projection failure after the event append could leave an event without an
  indexed row. Mitigation: return an error so callers retry, and keep projection
  replay/backfill as the repair path.
- Hydrating an actor after a fast-path create could differ from the returned
  OData body. Mitigation: add a test that creates through OData, reads through
  observe/actor state, and compares status/fields/sequence.

### DST Compliance

This touches `temper-server`, a simulation-visible crate. The fast path uses the
same deterministic scheduler primitives as `EntityActor` for event metadata
(`sim_now()` and `sim_uuid()`), keeps `BTreeMap`/`BTreeSet` conventions, and
does not introduce filesystem, network, thread, random, or wall-clock behavior
inside simulation-visible logic.

## Non-Goals

- No entity-name-specific `SessionEntry` shortcut.
- No bypass of Cedar/write prechecks.
- No bypass of event journaling.
- No bypass of query projection acknowledgement.
- No change to entities that have any IOA action or integration trigger.
- No OData `$batch` implementation in this slice.

## Alternatives Considered

1. **Add a TemperPaw-specific SessionEntry bulk endpoint** - Rejected because it
   would hardcode a product entity into framework code and weaken the platform
   mission.
2. **Use OData `$batch`** - Rejected for this slice because it still leaves each
   entity create on the full actor path unless the server also gets a data-only
   fast path.
3. **Remove SessionEntry read-back verification** - Rejected because the recent
   projection incidents proved that correctness must stay visible.
4. **Leave the path unchanged** - Safe, but Datadog shows this is now one of the
   larger remaining avoidable hot-path costs.

## Rollback Policy

Revert the fast-path branch in OData create handling and route all entity-set
creates back through `get_or_create_tenant_entity`. Persisted events and
projection rows written by the fast path use the same shape as the actor path, so
rollback does not require data migration.
