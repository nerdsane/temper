# ADR-0104: Projection Read Parity And Local Tenant Propagation

- Status: Proposed
- Date: 2026-05-18
- Deciders: Temper core maintainers
- Related:
  - ADR-0082: Projection Correctness Observability
  - ADR-0091: Query Projection Diff Index Upserts
  - ADR-0095: Projection Transaction Fast Path
  - ADR-0096: Set-Based Projection Index Reconciliation
  - ADR-0099: Local WASM TData Host Path
  - ADR-0103: Local TData Host Stream Contract Preservation
  - `crates/temper-server/src/odata/read.rs`
  - `crates/temper-server/src/state/dispatch/wasm/local_tdata_host.rs`
  - `crates/temper-store-postgres/src/store.rs`
  - `crates/temper-store-turso/src/store/entity_listing.rs`

## Context

Production investigation found a projection/read-model correctness violation:

```text
GET /tdata/DesignLanguages
  -> 6 rows

GET /tdata/DesignLanguages?$filter=Status eq 'Published'
  -> 173 rows
```

An unfiltered collection read must be a superset of every filtered collection
read for the same tenant and entity type. The current architecture can violate
that invariant because collection reads use two different ID sources:

- unfiltered collection reads start from `ServerState::list_entity_ids_lazy`;
- filtered reads can use SQL pushdown through the query plane;
- the two sources can be event-log IDs, `entity_catalog`, or
  `entity_field_index` depending on backend and route.

The frontend mitigation that queries each state separately is acceptable as an
emergency workaround, but it does not fix the root read-model contract.

A second production symptom appeared while inspecting or pausing cron jobs:

```text
Missing required X-Tenant-Id header
```

That error is emitted by the OData tenant resolver in multi-tenant mode. After
ADR-0103, `LocalTDataWasmHost` forwards eligible local `/tdata` calls directly
to in-process OData handlers. This path preserves the guest-supplied headers,
but does not synthesize `X-Tenant-Id` from the WASM execution context when the
guest omits it. A direct HTTP caller should still send the header explicitly,
but a local host optimization must not make internal same-tenant calls less
reliable than the external host path.

## Decision

Fix the projection read path and the local TData tenant contract in one
correctness-focused patch because both are read/control-plane routing contracts.

### Sub-Decision 1: Unfiltered Reads Use The Query Plane When Available

For durable query-plane backends, collection reads should derive unfiltered IDs
from the same authoritative live projection source used by filtered reads.

Postgres already uses `entity_catalog` for filter pushdown. It should also be
able to list entity IDs by type from `entity_catalog` when the query-plane
catalog is populated, rather than starting unfiltered reads from the event log.

Turso already has a catalog-first listing path, but the same invariant must be
tested against catalog and field-index divergence.

**Why this approach**: The projection API is a read-model API. If filtered reads
are served from the projection, unfiltered reads must not use a different stale
or narrower source. Falling back to the event log is still useful when the
query-plane catalog is empty or unavailable, but a non-empty projection catalog
must not be bypassed for unfiltered reads.

### Sub-Decision 2: Add Superset Regression Tests

Tests must prove:

- `All(entity_type)` includes every filtered subset returned by `Status`;
- unfiltered count is greater than or equal to each filtered count;
- a non-empty projection catalog with more rows than the event-source listing
  does not cause the unfiltered API to return a smaller set.

**Why this approach**: The production failure is a semantic contradiction, not
just a performance regression. The invariant should be executable and backend
agnostic enough to catch future planner changes.

### Sub-Decision 3: Local TData Synthesizes Missing Tenant Header From Context

`LocalTDataWasmHost` should carry the invocation tenant and add
`X-Tenant-Id: <tenant>` when a local `/tdata` call omits it. It must not
override an explicit guest header.

**Why this approach**: A same-process local transport optimization has access
to the invocation tenant through `ServerState` dispatch context. Internal calls
should be tenant-scoped by construction, while still preserving explicit caller
headers for tests and future cross-tenant administrative flows.

### Sub-Decision 4: Add Datadog-Facing Read Diagnostics

OData collection spans should expose enough fields to diagnose the next parity
issue without reconstructing it from DB queries:

- tenant;
- entity set and entity type;
- whether filter pushdown ran;
- materialization ID source;
- candidate count before pagination;
- returned count after query options.

**Why this approach**: Datadog already proved useful for latency and runtime
version attribution, but this incident shows we also need correctness counters
on the read path itself.

## Rollout Plan

1. **Phase 0 (Immediate)** - Add the ADR, regression tests, Postgres
   catalog-first listing support, local TData tenant synthesis, and OData read
   diagnostics.
2. **Phase 1 (Production proof)** - Deploy to TemperPaw and run live
   `DesignLanguages` all-vs-state queries plus a cron inspect/pause path that
   previously returned the missing-header error.
3. **Phase 2 (Monitoring)** - Add a Datadog monitor or scheduled parity probe
   that alerts when any filtered OData collection count exceeds the unfiltered
   collection count for the same entity type and tenant.

## Readiness Gates

- Regression tests fail before the fix and pass after it.
- Focused `temper-server` and store tests pass.
- `cargo fmt --all -- --check`, `cargo clippy`, and `git diff --check` pass
  for the touched crates.
- Production proof shows `All(DesignLanguages) >= Published(DesignLanguages)`.
- Production proof shows cron inspect/pause sends or synthesizes `X-Tenant-Id`
  and no longer returns the missing-header error.
- The latency report HTML records the incident, PR, test evidence, and live
  proof.

## Consequences

### Positive

- Restores OData collection semantics for projection-backed reads.
- Makes the projection API safe for frontends to consume without fragile
  state-by-state workarounds.
- Keeps local TData latency benefits while preserving tenant scoping.
- Gives Datadog enough read-path attributes to distinguish drift, pagination,
  and planner behavior.

### Negative

- A catalog-first unfiltered path trusts the query-plane projection more
  strongly, so projection replay/backfill health becomes even more important.
- Additional span fields add a small amount of telemetry cardinality, though
  the chosen fields are bounded by tenant and entity types already present in
  the system.

### Risks

- If `entity_catalog` has stale rows that the event log has already deleted,
  catalog-first listing could temporarily include too many IDs. Mitigation:
  keep deletion projection paths covered and add parity checks against replay.
- If local TData synthesizes the wrong tenant, internal calls could cross tenant
  boundaries. Mitigation: synthesize only from the invocation tenant and never
  default from global state in multi-tenant mode.
- If a caller intentionally omits `X-Tenant-Id` to test external HTTP contract
  behavior, local TData will now differ. Mitigation: this synthesis is only for
  in-process local TData host calls, not external HTTP requests.

### DST Compliance

- This touches `temper-server`, a simulation-visible crate.
- No new wall-clock time, random IDs, filesystem access, or background threads
  are introduced.
- Collection ordering remains deterministic through existing ordered SQL and
  `BTree*` usage.
- Tenant synthesis is derived from existing deterministic invocation context.

## Non-Goals

- Do not remove the frontend mitigation in this patch.
- Do not redesign projection replay, backfill, or event sourcing.
- Do not weaken the external HTTP requirement that multi-tenant OData callers
  provide `X-Tenant-Id`.
- Do not make local TData support cross-tenant administrative calls without an
  explicit header and policy review.

## Alternatives Considered

1. **Frontend-only workaround** - Rejected because it hides the contradiction
   and leaves other clients with incorrect `All` reads.
2. **Always read unfiltered IDs from the event log** - Rejected because filtered
   reads are already projection-backed and can then return supersets of
   unfiltered reads.
3. **Disable filter pushdown** - Rejected because it gives up the latency win
   and still leaves source divergence unresolved for catalog fast reads.
4. **Force every WASM guest to add tenant headers manually** - Rejected for
   local TData because the host already knows the invocation tenant and should
   preserve same-tenant execution semantics.

## Rollback Policy

If the projection listing change causes unexpected reads, revert the
catalog-first listing change while keeping the parity tests as ignored
regressions and run a projection replay/backfill. If tenant synthesis causes
unexpected local TData behavior, revert only the synthesis and require all
affected WASM guests to pass `X-Tenant-Id` explicitly until a narrower host
contract is approved.
