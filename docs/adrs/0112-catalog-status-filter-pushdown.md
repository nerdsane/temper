# ADR-0112: Catalog Status Filter Pushdown

- Status: Accepted
- Date: 2026-05-21
- Deciders: Temper core maintainers
- Related:
  - ADR-0111: Full-State Catalog Fast Read
  - `crates/temper-server/src/odata/filter_sql.rs`
  - `crates/temper-store-postgres/src/platform.rs`
  - `crates/temper-store-turso/src/store/field_index.rs`

## Context

ADR-0111 moved OData entity-set reads toward full-state catalog materialization so list endpoints can avoid per-entity actor hydration. Live Taxonomies proof on production version `5f360b6a566ff3539e8049b67e9c365a6b7768c0` showed that unfiltered reads now return the full 411 rows, but `Status eq 'Draft'` still returned zero while the unfiltered catalog response contained 18 Draft rows.

The mismatch exists because the OData SQL translator treats every property in `$filter` as a projected field and emits an `entity_field_index` membership subquery. `Status` is not merely a user field. It is the canonical entity state column on `entity_catalog` and is also duplicated onto field-index rows only as row metadata. A status filter must therefore be evaluated against the catalog status, not against a synthetic `field_name = 'Status'` field that may or may not exist in `fields`.

## Decision

### Sub-Decision 1: Treat `Status` As A Catalog Predicate

`Status eq ...` and `Status ne ...` translate to direct `entity_catalog.status` predicates. This keeps status filtering aligned with the source of truth used by full-state catalog reads.

**Why this approach**: The entity status is always present on the catalog row, even when the entity's projected field set omits a `Status` scalar. Filtering on the catalog column removes the drift between unfiltered reads and filtered reads without requiring every entity spec to duplicate status into fields.

### Sub-Decision 2: Keep Ordinary Field Filters On The EAV Index

Non-status fields continue to use `entity_field_index` membership subqueries. This preserves the existing pushdown behavior for user fields and avoids broadening the catalog table into an arbitrary JSON query engine.

**Why this approach**: The EAV table is still the correct acceleration structure for scalar user fields. Only canonical entity status has a first-class projection column.

### Sub-Decision 3: Align Postgres And Turso Filter Hosts

The Postgres query host already evaluates translated clauses from `entity_catalog`, which can combine direct catalog predicates with field-index subqueries. Turso will use the same catalog-hosted shape so behavior remains consistent across stores.

**Why this approach**: The translator should not need backend-specific knowledge beyond placeholder style. Both stores have `entity_catalog`, and a catalog-hosted query allows status predicates to include entities even if they have no scalar field-index rows.

## Rollout Plan

1. **Phase 0 (Immediate)** - Patch the filter translator and Turso query host. Add regression tests for `Status eq`, `Status ne`, and status filters returning catalog rows whose fields omit `Status`.
2. **Phase 1 (Production proof)** - Deploy through TemperPaw, rerun live Taxonomies proofs, and verify `All` includes the same statuses as filtered reads.
3. **Phase 2 (Observability)** - Keep Datadog trace tags for `filter_pushdown`, `id_source`, `candidate_count`, and `materialized_count` so status pushdown can be distinguished from unfiltered catalog reads.

## Readiness Gates

- `Status eq 'Draft'` returns the same Draft row count implied by unfiltered catalog reads.
- Filtered status reads materialize from the catalog path without full actor fanout.
- Existing scalar field filter tests continue to pass for Postgres and Turso-backed query stores.

## Consequences

### Positive

- Removes a projection correctness gap where `All` and filtered status reads gave contradictory answers.
- Makes the full-state catalog read model more trustworthy for UI consumers.
- Reduces pressure to add redundant `Status` fields to every entity's projected field map.

### Negative

- The filter translator now has knowledge of one canonical entity property.

### Risks

- Specs that intentionally define a user field named `Status` cannot use `$filter=Status ...` to target the field instead of the entity status. This is acceptable because OData `Status` has already behaved as an entity-level concept in the API surface.

### DST Compliance

- The change is deterministic: it only changes SQL predicate generation and does not introduce clocks, randomness, threads, or non-deterministic iteration.

## Non-Goals

- This ADR does not add JSONB predicate pushdown for arbitrary nested fields.
- This ADR does not redesign status naming or add a separate OData namespace for entity metadata.
- This ADR does not address unrelated projection lag or replay parity gaps.

## Alternatives Considered

1. **Backfill `fields.Status` for every entity** - Rejected because it duplicates canonical status and can drift from the catalog status column.
2. **Disable status filter pushdown and fall back to in-memory filtering** - Rejected because it would restore correctness but lose the latency improvement for status-scoped galleries.
3. **Use the `entity_field_index.status` metadata column only** - Rejected because entities with no scalar indexed fields would still be invisible to status filters.

## Rollback Policy

Revert the translator and Turso query-host changes. The system would return to the previous behavior where status filters depend on `field_name = 'Status'` rows.
