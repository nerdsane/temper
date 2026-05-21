# ADR-0116: OData Pushdown Sparse Page Planning

- Status: Accepted
- Date: 2026-05-21
- Deciders: Temper core maintainers
- Related:
  - ADR-0111: Full-State Catalog Fast Read
  - ADR-0112: Catalog Status Filter Pushdown
  - ADR-0115: OData Selected Catalog Projection
  - `crates/temper-server/src/odata/read.rs`
  - `crates/temper-server/src/odata/read_support.rs`
  - `crates/temper-store-postgres/src/selected_catalog.rs`

## Context

Datadog on production version `698ef8579ae0c071604e1a54062d95298ac03e0f`
shows that `SessionEntries` OData collection reads remain a major latency tail.
Retained traces for `odata.entity_set_read` show `candidate_count` around
`658-663`, `filter_pushdown=true`, `id_source=filter_pushdown`, and durations
around `3.0-3.9 s`. The dominant child span is the Postgres catalog load:

```sql
SELECT entity_id, status, fields, state, sequence_nr
FROM entity_catalog
WHERE tenant = $1 AND entity_type = $2 AND entity_id = ANY($3)
ORDER BY entity_id
```

One representative trace is
`https://app.datadoghq.com/apm/trace/6cf011aaab2ebdec070ae65f235340c7`.
The route-message WASM path issues this request shape:

```text
/tdata/SessionEntries?$filter=SessionId eq '{session}'&$orderby=Sequence desc&$top=1
```

The filter pushdown correctly narrows the universe to the session's entries,
but Temper still loads full `entity_catalog.state` JSON for every matching row
before applying `$orderby` and `$top`. For long sessions this means reading and
hydrating hundreds of large SessionEntry states to return a single row.

This is not an inherent Temper architecture limit. It is a missing query-shape
optimization in the OData read planner.

## Decision

Add a sparse page-planning path for pushed-down OData collection reads.

When a filter has already been translated to the query projection, catalog
coverage is complete, `$expand` is absent, and the request has pagination or
ordering, Temper may use the selected-catalog projection to materialize only the
fields needed to choose the page:

- `entity_id`, always, so final IDs can be recovered without parsing
  `@odata.id`
- properties referenced by `$filter`, so the pushdown result can still be
  verified in memory
- properties referenced by `$orderby`, so sorting uses the same JSON comparison
  rules as the existing in-memory evaluator

After applying `$filter`, `$orderby`, `$skip`, `$top`, and `$count` to this
sparse candidate set, Temper hydrates only the selected page IDs for the final
response. If the original query has `$select`, the final hydration can use the
same selected-catalog projection introduced in ADR-0115; otherwise it returns
the normal full OData entity shape.

### Why This Approach

This keeps correctness ahead of speed:

- the SQL filter still determines the candidate IDs;
- the sparse candidate rows are re-filtered and sorted by the existing OData
  in-memory evaluator;
- catalog coverage must be complete, otherwise the planner falls back to the
  existing materialize-all behavior that can repair missing projection rows;
- the final response is built from the already chosen page IDs, preserving
  OData `$skip`/`$top` semantics without applying pagination twice.

It also stays generic. The planner is not a `SessionEntry` special case and
does not hardcode `SessionId` or `Sequence`; it reads the properties directly
from the parsed OData query.

## Rollout Plan

1. **Phase 0 (Immediate)** - Add the sparse page planner to Temper's OData read
   path, with span fields recording whether the planner ran and how many sparse
   candidates/final page rows it used.
2. **Phase 1 (Rollout)** - Bump TemperPaw to the merged Temper commit, deploy,
   and prove with the route-message `SessionEntries` query.
3. **Phase 2 (Follow-up)** - If Datadog still shows route-level ambiguity, add
   HTTP route span tags for OData query shape so selected/full/filter/order
   families can be aggregated without child-span joins.

## Readiness Gates

- Existing OData filter/order/top behavior remains covered by focused tests.
- New tests prove sparse planning extracts filter and order fields, preserves
  count and final ID order, and avoids applying pagination twice.
- Local `cargo fmt`, focused OData tests, `cargo check -p temper-server`, and
  `cargo clippy -p temper-server --all-targets -D warnings` pass.
- Production after proof records `SessionEntries` pushed-down reads with
  `pushdown_sparse_page=true`, candidate probe count near the session size, and
  final `materialized_count` near the requested page size.
- Live request proof shows correctness: the latest `SessionEntry` returned by
  the optimized route matches the unoptimized full query semantics.

## Consequences

### Positive

- Large filtered-and-ordered collection reads no longer need to fetch full state
  for every candidate when only a small page is needed.
- The route-message hot path can fetch the latest SessionEntry without paying
  multi-second catalog materialization on long sessions.
- The same planner can help other filtered OData queries that combine `$filter`
  with `$top`, `$skip`, or `$orderby`.

### Negative

- The read planner gains a two-phase path: sparse candidate planning followed
  by final row hydration.
- Count semantics require care because `$count` is computed from sparse
  candidates before final pagination.

### Risks

- Projection drift could cause sparse candidate planning to choose the wrong
  page. Mitigation: only use the planner when catalog coverage is complete,
  re-evaluate the filter/order in memory over sparse JSON values, keep replay
  parity probes, and fall back to the old path when coverage is incomplete.
- Query shapes with `$expand` can require full entity bodies before pagination.
  Mitigation: `$expand` is excluded from this path.

### DST Compliance

- The change touches `temper-server`, a simulation-visible crate.
- Determinism is preserved: candidate ordering comes from the existing ordered
  ID vector and `apply_query_options`; property sets use `BTreeSet` for stable
  ordering; no wall-clock, random, thread, or external I/O source is added.

## Non-Goals

- This ADR does not push generic `$orderby` into SQL.
- This ADR does not change the query projection schema.
- This ADR does not add entity-specific SessionEntry indexes or hardcoded
  SessionEntry behavior.
- This ADR does not remove the catalog coverage fallback path.

## Alternatives Considered

1. **Hardcode a SessionEntry latest-entry endpoint** - Rejected because it would
   improve one caller while bypassing OData and weakening the platform surface.
2. **Push `$orderby` directly into SQL for all fields** - Rejected for this slice
   because `entity_field_index.field_value` is text, while OData sorting uses
   JSON-aware numeric/string comparison. A generic SQL implementation needs more
   type metadata and a wider design.
3. **Pre-truncate pushed-down IDs before re-filtering** - Rejected because it
   could hide correct rows if projection drift or stale field-index data creates
   false positives before the actual page.

## Rollback Policy

Disable by reverting this PR. The old materialize-all pushed-down path remains
the fallback whenever sparse page planning is not eligible or catalog coverage
is incomplete.
