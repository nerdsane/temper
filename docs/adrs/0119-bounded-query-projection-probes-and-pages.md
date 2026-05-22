# ADR-0119: Bound Query Projection Probes And Pages

- Status: Proposed
- Date: 2026-05-22
- Deciders: Temper core maintainers
- Supersedes: ADR-0117 for unbounded sparse-page materialization
- Related:
  - ADR-0114: Bounded Projection Replay Parity Probe
  - ADR-0117: OData Pushdown Sparse Page Planning
  - ADR-0118: OData Pushdown Planner Engagement Observability
  - `crates/temper-server/src/odata/read.rs`
  - `crates/temper-server/src/state/projection_backfill/replay_parity.rs`
  - `crates/temper-store-postgres/src/platform.rs`

## Context

The latency program added projection-backed OData pushdown and projection replay
parity probes. Both were intended to make reads faster and correctness more
observable, but two paths were still bounded only after allocation:

1. Sparse OData page planning accepted all SQL-pushed candidate IDs and then
   materialized sparse JSON rows for every candidate before applying `$orderby`,
   `$skip`, and `$top`.
2. The replay parity observe endpoint accepted a `limit`, but the verifier
   first called `list_entity_ids(...)` for the whole tenant and truncated the
   result afterwards.

Datadog showed production candidate sets in the tens of thousands and
temperpaw RSS rising to about 8 GiB. The architecture should never require a
read or observability probe to allocate the full tenant/entity working set just
to return a small page or bounded sample.

## Decision

### Store-Level Bounds Before Allocation

Add bounded storage APIs for projection and journal reads:

- `QueryPlaneStore::query_field_index_page(...)` returns a filtered, ordered
  page of entity IDs plus an optional total count.
- `EventStore::list_entity_ids_limited(...)` returns at most the requested
  number of authoritative journal entity IDs.

The limit is part of the storage query, not a post-processing step.

### Postgres Paged OData Pushdown

For translated OData filters with a page shape, Postgres should answer the page
directly from `entity_catalog`:

- filter via the existing SQL-translated predicate;
- order by `status`, `entity_id`, or typed JSONB projection fields;
- apply `LIMIT` and `OFFSET` in SQL;
- return only the final page IDs to hydrate.

Sparse in-process planning remains a fallback for smaller candidate sets and
for storage backends that cannot yet page in SQL. The fallback is no longer
allowed to materialize an unbounded pushed-down set.

### Replay Parity Limit Moves Into The Store

When the observe replay-parity endpoint is bounded, the verifier calls the new
limited journal listing API. The existing full manual verifier remains
available for explicit operator runs, but the HTTP observe probe does not pay
the full-tenant allocation cost.

## Rollout Plan

1. **Phase 0 (Immediate)** - Ship bounded Postgres page reads, bounded replay
   parity listing, regression tests, and Datadog fields that show page counts
   rather than candidate materialization.
2. **Phase 1 (Follow-up)** - Add equivalent native paged projection reads for
   Turso if production needs that backend for large OData pages.
3. **Phase 2 (Production proof)** - Verify RSS stability, absence of repeated
   Railway restarts, bounded query span counts, and unchanged read correctness
   on the hot SessionEntries workload.

## Readiness Gates

- Large filtered `$orderby ... &$top=1` reads do not materialize all candidate
  projection rows.
- Replay parity HTTP probes allocate no more than the requested entity limit.
- Local tests cover the page API and the replay parity bounded path.
- Production Datadog confirms no recurring 7-8 GiB RSS spikes after deploy.

## Consequences

### Positive

- The hot latest-row OData read becomes both faster and memory bounded.
- Observe probes stop being able to OOM the service they are measuring.
- The page contract is explicit and testable at the storage boundary.

### Negative

- Generic OData ordering in SQL must mirror the in-memory comparator for the
  common scalar projection types.
- Backends without a native page API may still fall back to smaller bounded
  in-process planning until they implement the native capability.

### Risks

- A projection drift can still make a projection-backed page incomplete. That
  is handled by separate projection parity and lag/drift observability rather
  than by unbounded per-read full-tenant coverage checks.
- Some less common OData order expressions may continue to use the bounded
  fallback path until they are pushed down natively.

### DST Compliance

- The changed server paths remain production I/O paths and keep existing
  deterministic simulation boundaries.
- New store methods preserve deterministic ordering by returning ordered IDs
  before limiting.

## Non-Goals

- This ADR does not redesign projection correctness or replay scheduling.
- This ADR does not remove the full manual replay parity verifier.

## Alternatives Considered

1. **Raise container memory** - Rejected. It masks the unbounded allocation and
   would fail again as tenant size grows.
2. **Disable all sparse planning** - Rejected. It removes the latency win and
   still leaves fallback collection reads vulnerable unless they are bounded.
3. **Keep truncating after allocation** - Rejected. The failure mode is the
   allocation itself.

## Rollback Policy

Disable the paged pushdown path through code rollback and keep the bounded
fallback caps in place. Replay parity can be limited operationally by lowering
the observe endpoint limit or disabling scheduled probes.
