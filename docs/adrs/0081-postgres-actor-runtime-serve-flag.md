# ADR-0081: Postgres Actor Runtime Serve Flag

- Status: Accepted
- Date: 2026-05-11
- Deciders: Temper core maintainers
- Related:
  - ADR-0066: Storage stack backend selection
  - ADR-0070: Postgres multitenant isolation
  - `crates/temper-cli/src/serve`
  - `crates/temper-actor-runtime`
  - `crates/temper-server/src/odata`

## Context

PR #218 introduced the Postgres-backed actor runtime and OData branches that can
read and write through `odp_temper.actor_instances` and
`odp_temper.actor_messages`. That work proved the runtime path, but normal
`temper serve` startup still constructs only the legacy in-process actor system.
Operators can select Postgres for platform persistence with
`TEMPER_EVENT_STORE=postgres`, but there is no supported flag that enables the
Postgres actor runtime and wires OData entity types to it.

This creates an unsafe ambiguity: Postgres can be the persistence backend while
entity execution still happens through the legacy runtime. The startup contract
must make the actor runtime explicit, reuse the same database selected for the
rest of the platform, and avoid silently moving unsupported specs onto the new
runtime.

## Decision

### Sub-Decision 1: Add an explicit actor runtime selector

`temper serve` gains an actor runtime selector with values `legacy` and
`postgres`. The default remains `legacy`. `TEMPER_ACTOR_RUNTIME` may select the
runtime when the CLI flag is omitted, mirroring `TEMPER_EVENT_STORE`.

**Why this approach**: Runtime selection is a deployment property, not an
implicit side effect of choosing a storage backend. Keeping the default legacy
preserves existing behavior while giving operators a direct canary switch.

### Sub-Decision 2: Postgres actor runtime requires Postgres storage

The `postgres` actor runtime is valid only when the selected storage backend is
Postgres and `DATABASE_URL` is configured. The runtime uses that same database
for its `odp_temper` actor tables.

**Why this approach**: One Postgres database remains the source for platform
metadata, event storage, query projections, and durable actor state. Splitting
actor state to a second Postgres URL would complicate backup, tenant isolation,
and failure recovery without solving a current requirement.

### Sub-Decision 3: Actor-backed entity types are explicit and validated

Operators may repeat `--actor-backed-type TYPE` or set
`TEMPER_ACTOR_BACKED_TYPES=TYPE1,TYPE2`. A type may be scoped to one tenant with
`tenant:TYPE`. When no type list is supplied and the Postgres actor runtime is
selected, startup attempts to back every registered entity type. Startup fails
if a selected type is not loaded, has conflicting IOA sources across selected
tenants, or uses effects the current `SpecDrivenActor` cannot execute
correctly.

**Why this approach**: The current actor runtime has one handler per actor type,
so it cannot safely run two different selected IOA definitions with the same
entity type name. Tenant-scoped selection lets operators canary one tenant while
leaving same-named types in other tenants on the legacy runtime. The actor
runtime also does not yet implement every legacy dispatch side effect. Failing
early is safer than routing production traffic through partial semantics.

### Sub-Decision 4: Start the scheduler at server boot

When enabled, `temper serve` starts the Postgres actor scheduler in the
background after registering handlers. Synchronous OData writes still call
`activate_now`, while the scheduler drains follow-up messages produced by actor
effects.

**Why this approach**: Direct request handling remains low latency for the
source actor, and queued actor-to-actor messages make progress without requiring
the caller to poll manually.

## Rollout Plan

1. **Phase 0 (Immediate)** — Add the CLI/env selectors, schema bootstrap, handler
   registration, OData type selection, scheduler startup, and focused tests.
2. **Phase 1 (Follow-up)** — Extend `SpecDrivenActor` support for additional
   effects such as lists, scheduled actions, spawn, and adapter/WASM triggers.
3. **Phase 2** — Promote selected production apps after their specs pass the
   actor-runtime compatibility gate.

## Readiness Gates

- `temper serve --storage postgres --actor-runtime postgres` must reject missing
  `DATABASE_URL`.
- The actor runtime must use the same `DATABASE_URL` as Postgres storage.
- Selected actor-backed entity types must be registered before OData routes can
  dispatch to them.
- Live Postgres OData create/action/read must pass through
  `odp_temper.actor_instances`.

## Consequences

### Positive

- Operators can actually use the Postgres actor runtime from `temper serve`.
- Runtime selection is visible in startup configuration and logs.
- Unsupported specs fail at startup instead of failing after traffic is routed.

### Negative

- Specs that rely on unsupported side effects cannot be actor-backed until the
  runtime implements those effects.
- Multi-tenant deployments with divergent IOA definitions for the same entity
  type must stay on legacy runtime or rename the entity types.

### Risks

- A deployment may expect `--actor-runtime postgres` to move every app, but a
  compatibility failure can block startup. The mitigation is to canary with
  explicit `--actor-backed-type` entries and expand the set after verification.

### DST Compliance

- The server keeps legacy runtime as the default, so simulation-visible behavior
  is unchanged unless the new runtime is explicitly selected at startup.
- Environment reads happen once at CLI startup.
- Actor-runtime scheduler polling is a production I/O path, not a simulation
  scheduler path.

## Non-Goals

- Implement every legacy entity side effect in `SpecDrivenActor`.
- Add a second actor-specific Postgres connection string.
- Change the default runtime for existing deployments.
- Support divergent same-named entity specs across tenants in the current
  actor-runtime handler registry.

## Alternatives Considered

1. **Enable PG actor runtime whenever storage is Postgres** — Rejected because it
   would silently change execution semantics for existing apps.
2. **Separate `ACTOR_DATABASE_URL`** — Rejected because it splits source of truth
   and complicates operational recovery before there is a scaling need.
3. **Route every spec without compatibility validation** — Rejected because the
   current `SpecDrivenActor` intentionally implements only a subset of effects.

## Rollback Policy

Set `--actor-runtime legacy` or unset `TEMPER_ACTOR_RUNTIME`. Actor rows remain
in `odp_temper` but are ignored by the legacy runtime.
