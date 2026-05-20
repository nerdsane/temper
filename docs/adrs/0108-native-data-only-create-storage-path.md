# ADR-0108: Native Data-Only Create Storage Path

- Status: Proposed
- Date: 2026-05-19
- Deciders: Temper core maintainers
- Related:
  - ADR-0081: Latency Observability Acceleration Program
  - ADR-0091: Query Projection Diff Index Upserts
  - ADR-0095: Projection Transaction Fast Path
  - ADR-0096: Set-Based Projection Index Reconciliation
  - ADR-0105: Data-Only Entity Create Fast Path
  - ADR-0106: WASM Integration Envelope Attribution
  - ADR-0107: WASM Lazy Secret Authorization
  - `crates/temper-server/src/state/entity_ops.rs`
  - `crates/temper-server/src/storage/mod.rs`
  - `crates/temper-store-postgres/src/data_only_create.rs`
  - `crates/temper-store-postgres/src/platform.rs`
  - `crates/temper-store-postgres/src/store.rs`

## Context

PERF-030 removed the first dominant WASM dispatch envelope phase by changing eager all-secret authorization into bounded bootstrap secrets plus lazy per-key authorization. The fixed-version production after batch on `sha-2b1f1a9` moved `dispatch.wasm.phase.authz_secret_resolution` from about `78.7 ms` average / `87.8 ms` p95 to about `14.0 ms` average / `15.1 ms` p95.

The next production trace shape is now storage-bound. In the PERF-030 after batch `perf-030-after-batch-20260519121906`, `provider_response_applier` still spends about `99.7 ms` average in `dispatch.wasm.phase.engine_invoke_and_handle`, while Datadog span metrics show only about `1-2 ms` of busy time and about `93-106 ms` of idle wait per invocation. A representative trace (`5b6c886d955b3cd8f3b065010b085a2f`) shows the large child as local `POST /odata/{path}` for SessionEntry creation through `entity.create_data_only_tenant_entity_fast_path`, around `59-64 ms`.

The current data-only path avoids actor hydration, but it still composes two generic storage operations:

1. Append the first `Created` event through the generic event journal path, including a `SELECT COALESCE(MAX(sequence_nr), 0)` and an `INSERT INTO events` in one transaction.
2. Acknowledge query projection synchronously through the generic projection upsert path, including a catalog `SELECT ... FOR UPDATE`, a catalog insert/update, field-index delete reconciliation, field-index insert/upsert, and commit.

This is correct, but it is over-general for a brand-new data-only entity. On a new entity, Temper already knows the first event sequence is `1`, the projection has no previous field-index rows, and the event append plus projection acknowledgement must succeed or fail atomically for the fast path to count as a create acknowledgement.

## Decision

Add a native storage capability for brand-new data-only entity creation. For storage backends that implement it, the server data-only fast path will persist the first event and the initial query projection in one storage transaction instead of composing generic append plus generic projection upsert.

### Sub-Decision 1: Capability Boundary

Introduce a server storage trait such as `DataOnlyCreateStore` and attach it to `StorageStack` as an optional capability. The method should accept the already-derived tenant, entity type, entity id, status, full fields, scalar projection fields, and first `Created` event envelope.

**Why this approach**: The optimization is storage-specific and should not pollute the general `EventStore` trait with a method only valid for first-event creates. Keeping it as an optional capability preserves existing Redis/Turso/sim behavior and lets unsupported backends fall back to the current generic path.

### Sub-Decision 2: PostgreSQL Native Path

Implement the native capability for `PostgresEventStore` with one transaction:

1. Acquire a connection and begin a transaction.
2. Insert the first event at `sequence_nr = 1`.
3. Insert the `entity_catalog` row with `projection_version = 2` and a projection hash.
4. Insert scalar `entity_field_index` rows using set-based `unnest`.
5. Commit.

Duplicate event or catalog keys must surface as a concurrency conflict so the caller can decline the fast path and fall back to the existing actor/generic path. Any other failure must abort the transaction and return an error so callers do not receive a false 201 acknowledgement.

**Why this approach**: It removes generic read-before-write and delete-reconciliation queries from the hot first-create path while keeping the same durable event journal, full catalog row, scalar index, tenant isolation, and all-or-nothing acknowledgement semantics.

### Sub-Decision 3: Observability Contract

The native path must emit a distinct span/resource name and preserve the existing event append, projection update, and Postgres transaction metrics where practical. The dashboard must record before and after Datadog evidence:

- Before: PERF-030 after batch on `sha-2b1f1a9`, especially `entity.create_data_only_tenant_entity_fast_path`, `POST /odata/{path}`, `provider_response_applier engine_invoke_and_handle`, and Session correctness read-back.
- After: fixed-version production batch for the new revision, the same span distributions, exact deployment SHA, zero error spans, and exact SessionEntry read-back.

**Why this approach**: The prior slices showed that removing work is not enough. The program only counts the change as a performance win if Datadog and live correctness evidence prove it.

## Rollout Plan

1. **Phase 0 (Immediate)** - Add the native optional storage capability, Postgres implementation, server fast-path call site, unit/integration coverage, and dashboard status.
2. **Phase 1 (Rollout)** - Merge Temper, bump TemperPaw to the exact Temper commit, run local Paw checks, open a rollout PR, and deploy the fixed version.
3. **Phase 2 (Production Proof)** - Run a controlled live mock-provider Session proof plus a five-run batch, read back exact SessionEntries, collect Datadog before/after spans and logs, and accept or reject the latency claim in the dashboard.

## Readiness Gates

- Native create returns the same OData acknowledgement shape as the existing data-only path.
- Duplicate creates do not corrupt events or projections.
- Projection read-back sees the new entity immediately after the create acknowledgement.
- Generic fallback still works for unsupported backends, action-bearing entities, non-object payloads, and concurrency conflicts.
- Tests cover event journal row, catalog row, scalar field index row, duplicate conflict, and fallback behavior.
- Production proof shows lower `entity.create_data_only_tenant_entity_fast_path` and lower provider-response storage wait without new projection drift or missing SessionEntry rows.

## Consequences

### Positive

- Removes several serial Postgres round trips from new data-only creates.
- Preserves correctness by keeping event and projection acknowledgement atomic.
- Reduces the hot SessionEntry materialization cost currently visible inside `provider_response_applier`.
- Keeps the optimization behind an explicit storage capability instead of hardcoding PostgreSQL assumptions into OData or WASM code.

### Negative

- Adds a storage-specific capability beside the generic event and query-plane traits.
- Creates a second write implementation that must stay aligned with projection schema changes.
- Does not address remaining host-chain construction, callback dispatch, or workflow drain costs.

### Risks

- A bug in the native path could create event/projection drift. Mitigation: one transaction, projection read-back tests, duplicate conflict tests, and live SessionEntry read-back.
- Metrics could fragment across native and generic paths. Mitigation: keep existing high-level data-only span and add a low-cardinality native storage span/resource.
- Backend parity could regress. Mitigation: make the capability optional and keep generic fallback as the default.

### DST Compliance

- `temper-server` is simulation-visible, so the call site must preserve deterministic state construction and ordering.
- Production-only timing uses existing `Instant`-based metric patterns with `// determinism-ok` comments where new timers are introduced.
- No wall-clock time or random UUID sources are added beyond the existing `sim_now()` and `sim_uuid()` event envelope construction.

## Non-Goals

- Do not make all query projection updates asynchronous.
- Do not weaken projection acknowledgement for SessionEntry creates.
- Do not remove the event journal.
- Do not change Cedar governance, tenant isolation, or spec-governed action semantics.
- Do not optimize non-data-only or action-bearing entity creates in this slice.

## Alternatives Considered

1. **Only add more instrumentation** - Useful but insufficient now that Datadog identifies the stacked storage path clearly enough for a targeted PR. Rejected as a standalone slice, though the implementation will preserve observability.
2. **Make SessionEntry projection eventually consistent** - Faster, but it risks exactly the data-drift class the program is trying to prevent. Rejected for this slice.
3. **Optimize only generic projection upsert** - Safer and smaller, but it leaves event append and first-create assumptions unused. Rejected as the primary approach because the measured hot path is a new-entity create where a native atomic path can remove more serial work.
4. **Create a TemperPaw-only batch endpoint** - Could reduce WASM host calls, but it would duplicate platform write semantics in the app. Rejected until the platform-native path is proven or disproven.

## Rollback Policy

Disable use of the optional native capability in the server data-only fast path and fall back to the current generic append plus projection upsert. Because the native path uses the same events, catalog, and field-index tables, no data migration is required for rollback.
