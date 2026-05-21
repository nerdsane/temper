# ADR-0115: OData Selected Catalog Projection

- Status: Accepted
- Date: 2026-05-21
- Deciders: Temper core maintainers
- Related:
  - ADR-0111: Full-state catalog fast reads
  - ADR-0112: Catalog status filter pushdown
  - ADR-0113: Catalog status state parity
  - ADR-0114: Bounded projection replay parity probe
  - `crates/temper-server/src/odata/read.rs`
  - `crates/temper-server/src/odata/read_support.rs`

## Context

The catalog fast-read series removed the largest OData collection bottleneck:
per-row actor hydration. Production reads now show a different latency shape.
For `GET /tdata/DesignLanguages` on `sha-7420fee`, an unselected 100-row read
returns about `1.04 MB` and takes roughly `180-267 ms` from the client. The same
collection with `$select=Id,Slug,Name,Status` returns about `13.7 KB` and takes
about `100 ms`.

That proves clients can reduce wire cost dramatically with `$select`, but the
server still builds each full catalog entity body first, scans it for field
overflow blob references, and only then applies `$select`. For list surfaces
such as galleries and dashboards, the requested shape is usually a small set of
scalar fields. Building the full response body before pruning is overlooked
work, not a limitation of Temper's verified actor architecture.

A body-only projection is not enough. If the materializer still fetches
`entity_catalog.state` and full `entity_catalog.fields` for every selected row,
the response is smaller but the database boundary still moves most of the old
payload. Production runs for the selected `DesignLanguages` list therefore need
the optimized path to be visible in both client bytes and Datadog span duration.

## Decision

When an entity-set read has a `$select` and no `$filter`, `$orderby`, or
`$expand`, catalog-backed materialization may project the catalog row directly
to the selected OData shape.

### Sub-Decision 1: Project Only Safe Query Shapes

Selected catalog projection applies only when:

- `$select` is present.
- `$filter` is absent.
- `$orderby` is absent.
- `$expand` is absent.

Pagination (`$top`, `$skip`) and `$count` remain compatible because the read
path already applies them to the ID set before materialization.

**Why this approach**: In filtered or ordered queries, the full entity body may
still be needed to re-check predicates or sort keys after SQL pushdown. Limiting
this optimization to select-only list reads keeps the change small and
correctness-preserving.

### Sub-Decision 2: Preserve OData Select Semantics

The projected body resolves selected properties using the same precedence as
the existing in-memory selector:

1. top-level entity state fields such as `entity_id`, `entity_type`, `status`,
   `sequence_nr`, `item_count`, `counters`, `booleans`, `lists`, and `fields`;
2. the projected `fields` object;
3. OData annotations, especially `@odata.id`.

The normal `apply_query_options` pass still runs after materialization, so the
new path is semantically idempotent with the old path.

**Why this approach**: Clients should not observe a response-shape difference
between actor materialization, full-state catalog materialization, and selected
catalog materialization except that unselected fields are absent as requested.

### Sub-Decision 3: Hydrate Only The Selected Shape

Field-overflow blob hydration runs on the projected selected body, not on the
full catalog state, when this optimization is active.

**Why this approach**: If a selected field contains a blob reference, the client
still receives hydrated data. If an unselected field contains a large blob
reference, the server does not fetch or scan it.

### Sub-Decision 4: Load Sparse Catalog Rows In Postgres

Postgres implements a `load_selected_entity_catalog_rows` query-plane
capability. For safe selected collection reads, it returns only:

- `entity_id`;
- `status`;
- `sequence_nr`;
- a JSON object containing requested properties resolved from
  `state -> prop`, `state -> 'fields' -> prop`, then `fields -> prop`.

The sparse row intentionally omits full `state`. Backends without this native
capability return `None`, and the read path falls back to the full catalog row
loader.

**Why this approach**: Production TemperPaw uses Postgres, so this gives the
live latency slice a real database-boundary reduction immediately while keeping
Turso and routed stores correct through the existing fallback. The selected
query preserves arbitrary JSON values; it does not rely on the scalar
`entity_field_index`, which would lose nested JSON and long non-indexed fields.

### Sub-Decision 5: Skip Full Shadow Drift Checks For Sparse Rows

Catalog shadow drift checks compare a catalog row against the actor's full
projected state. Sparse selected rows are intentionally partial, so they are not
eligible for that full-row shadow comparison. Full catalog reads and the replay
parity probe continue to cover projection correctness.

**Why this approach**: Running the existing full drift comparator on a partial
row would create false drift warnings. Fetching a second full row only for
shadow checks would reintroduce the latency work this slice removes.

### Sub-Decision 6: Add Trace Attribution

The `odata.entity_set_read` span records whether selected catalog projection was
used and how many selected properties were requested.

**Why this approach**: The latency program needs before/after proof in Datadog.
The span tags let us distinguish a genuinely optimized selected read from an
unselected read or a selected read that fell back to the full materializer.

## Rollout Plan

1. **Phase 0 (Temper PR)** - Add the selected catalog projection helper,
   connect it to entity-set materialization, add the Postgres sparse catalog
   loader, add tests, and record span fields.
2. **Phase 1 (TemperPaw rollout PR)** - Bump Temper in TemperPaw and deploy to
   Railway.
3. **Phase 2 (Live before/after proof)** - Re-run the production
   `DesignLanguages?$select=Id,Slug,Name,Status` sample and compare client
   timings plus Datadog `odata.entity_set_read` spans.
4. **Phase 3 (Adoption guidance)** - Keep frontend and agent surfaces on
   explicit `$select` for gallery/list views instead of relying on full entity
   collection reads.

## Readiness Gates

- Unit tests prove selected catalog projection preserves selected top-level and
  field values while omitting unselected large fields.
- OData read tests continue to pass for unselected, filtered, and patched reads.
- Datadog spans show `catalog_select_projection=true` only for safe select-only
  collection queries.
- Live selected `DesignLanguages` benchmark improves without changing row count
  or selected response fields.

## Consequences

### Positive

- Selected list reads avoid unnecessary full-body construction and blob scans.
- Gallery/dashboard surfaces have a clear fast path that does not weaken
  projection correctness.
- Datadog can attribute selected-list latency separately from full-list latency.

### Negative

- The collection materializer now has one more shape branch to test.
- This does not make unselected full collection reads smaller; clients still
  need `$select` for slim list views.

### Risks

- A selected field that is only available through an unusual future response
  extension could be missed. Mitigation: keep the existing full materializer for
  filtered, ordered, expanded, and non-selected reads, and make the helper mirror
  the current `select_fields` precedence.

### DST Compliance

This touches `temper-server`, a simulation-visible crate. It adds no wall-clock
time, randomness, filesystem I/O, network I/O, unbounded threads, or unordered
iteration. Existing environment reads remain the established startup-only OData
configuration gates.

## Non-Goals

- Do not change default OData collection response semantics.
- Do not infer a default `$select` for clients.
- Do not bypass projection parity checks or make the catalog the write
  authority.
- Do not optimize filtered or ordered selected reads until those shapes have a
  separate correctness plan.

## Alternatives Considered

1. **Lower the default OData page size** - Rejected because it changes response
   cardinality and can hide data without a next-link contract.
2. **Default every collection to a summary shape** - Rejected because it breaks
   existing OData semantics and clients that expect full entity states.
3. **Require frontend-only `$select` changes** - Helpful but incomplete because
   the server would still build and scan the full body before pruning.
4. **Optimize filtered selected reads immediately** - Rejected for this slice
   because predicate re-verification and ordering need a more careful field
   dependency analysis.

## Rollback Policy

Disable the selected projection branch by reverting the `catalog_select_projection`
call path. Existing full-state catalog materialization and actor fallbacks remain
unchanged.
