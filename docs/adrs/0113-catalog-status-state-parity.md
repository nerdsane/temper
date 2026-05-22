# ADR-0113: Catalog Status State Parity

- Status: Accepted
- Date: 2026-05-21
- Deciders: Temper core maintainers
- Related:
  - ADR-0111: Full-state catalog fast-read
  - ADR-0112: Catalog status filter pushdown
  - `crates/temper-server/src/state/projection_backfill.rs`
  - `crates/temper-server/src/state/entity_ops.rs`
  - `crates/temper-store-postgres/src/platform.rs`
  - `crates/temper-store-turso/src/store/field_index.rs`

## Context

ADR-0111 moved high-volume OData collection reads to the durable
`entity_catalog` row so unfiltered reads can return full entity state without
hydrating every actor. ADR-0112 then made `$filter=Status eq ...` bind directly
to `entity_catalog.status` so lifecycle filters use the same fast catalog path
instead of a fragile EAV field-index row.

The first live rollout proved that approach for populated statuses like
`Published` and `Archived`, but it also exposed drift for default states:
unfiltered Taxonomies returned full-state rows whose `status` and
`fields.Status` were `Draft`, while `Status eq 'Draft'` returned zero rows.
Live probing showed those Draft rows were being served by the event/actor
fallback during unfiltered reads, while SQL pushdown only consulted
`entity_catalog`. In other words, the durable catalog can be incomplete for
default-state sequence-1 rows during or after projection drift.

Status is not merely a display field. It is the entity lifecycle state used by
Cedar authorization, available-actions enrichment, workflow guards, and OData
filter pushdown. A fast read model is only safe if the indexed status column is
derived from the same canonical state that is returned to clients.

## Decision

Every catalog write must canonicalize `entity_catalog.status` from the projected
full state whenever the projected state contains a valid top-level lifecycle
status. The explicit status argument remains the fallback for legacy callers and
rolling deploys, but it is no longer allowed to win over the stored state body.

Filtered reads that use SQL pushdown must also verify catalog coverage against
the event-backed entity id set. If the catalog is missing rows, the read path
hydrates only those missing IDs through the actor/event path, applies the filter
to that small repair set in memory, returns matching entities, and repairs their
catalog rows for future pushdown reads.

### Sub-Decision 1: Canonicalize At The Store Boundary

The Postgres and Turso query-projection upsert paths will derive the status that
is written to `entity_catalog.status`, `entity_field_index.status`, and the
projection hash from:

1. `state.status`, when present and non-empty.
2. The explicit `status` argument, otherwise.

**Why this approach**: The store boundary is the last common point shared by
live actor writes, background projection writes, startup replay/backfill, native
data-only creates, and repair tools. Fixing only one caller would leave another
path able to reintroduce drift.

### Sub-Decision 2: Preserve API Compatibility

The `upsert_projection` and native create APIs keep their current `status`
argument. Callers already compute status and passing it remains useful for old
rows or projected states that do not yet contain a state body. The store simply
treats that argument as a fallback rather than the authoritative value.

**Why this approach**: This keeps the rollout small and avoids broad trait/API
churn while still making future drift much harder.

### Sub-Decision 3: Test Default-State Filter Parity

Tests must prove that a row whose full projected state is in a default state
like `Draft` is discoverable by `Status eq 'Draft'`, even if the fallback status
argument is stale. This directly captures the live failure mode.

**Why this approach**: The bug is dangerous because unfiltered reads and
filtered reads can both look individually healthy while contradicting each
other. A parity test anchors the contract.

### Sub-Decision 4: Treat Catalog Coverage As A Runtime Correctness Guard

When `$filter` is translated to SQL, the read path will compare the durable
catalog rows with the entity IDs known from the event/read-source union. Missing
catalog IDs are materialized and filtered separately, then their projections are
upserted as a repair side effect.

**Why this approach**: Startup backfill is still the primary healing mechanism,
but a filtered read must not return a wrong answer just because backfill has not
caught up or an older projection write was skipped.

## Rollout Plan

1. **Phase 0 (Immediate)** — Patch store canonicalization and read-time
   catalog coverage repair, add focused tests, and merge through Temper.
2. **Phase 1 (TemperPaw Rollout)** — Bump Temper in TemperPaw, deploy the new
   image, and verify `/paw/version` reflects the new build.
3. **Phase 2 (Live Repair Verification)** — Run live OData parity proof for
   Taxonomies: unfiltered count must equal the union of Draft, UnderReview,
   Published, and Archived filtered reads, with zero missing IDs.
4. **Phase 3 (Observability Evidence)** — Record Datadog traces and catalog
   parity/latency timings in the living dashboard.

## Readiness Gates

- Local tests prove catalog status canonicalization and OData filter parity.
- Live Taxonomies status union has no missing IDs.
- Datadog traces show the query stays on the catalog fast path.
- The living latency dashboard records before/after timings, PRs, deployment
  identifiers, and remaining risks.

## Consequences

### Positive

- Status filters use a fast indexed column without sacrificing correctness.
- Full-state catalog reads and filtered catalog reads agree.
- Backfills and repair jobs become capable of healing stale status columns.
- Translated filters stay correct while projection repair catches up.

### Negative

- Store writes inspect the projected state JSON before hashing/writing the row.
  This is a tiny cost compared with the projection write itself.
- Filtered reads perform an extra catalog coverage check against the entity ID
  set. Healthy catalogs pay a bounded batch-read cost; drifted catalogs hydrate
  only missing rows and then repair them.

### Risks

- A malformed projected state with an incorrect top-level `status` would now
  override the fallback argument. This is acceptable because the state body is
  already the canonical object returned by fast reads; parity requires the
  indexed column to match it.

### DST Compliance

- The change touches `temper-server` tests and storage crates, but does not
  introduce wall-clock time, randomness, threading, environment reads, or
  nondeterministic collections.

## Non-Goals

- Do not special-case `Draft` or any TemperPaw-specific entity type.
- Do not change the OData status filter translator introduced by ADR-0112.
- Do not relax projection correctness or hide drift with frontend fallbacks.

## Alternatives Considered

1. **Frontend or API fallback for Draft** — Rejected because it preserves the
   underlying projection drift and would fail for the next default state.
2. **Status-column SQL fallback to `fields.Status`** — Rejected because it
   makes every status predicate more complex and defeats the clean lifecycle
   index contract.
3. **Broad API refactor removing the status argument** — Rejected for this
   patch because it increases rollout risk without adding correctness beyond
   boundary canonicalization.

## Rollback Policy

Revert the store-boundary canonicalization patch and keep ADR-0112's status
pushdown disabled or rolled back until a replacement parity strategy is ready.
