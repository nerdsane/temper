# ADR-0091: Query Projection Diff Index Upserts

- Status: Proposed
- Date: 2026-05-16
- Deciders: Temper core maintainers
- Related:
  - ADR-0082: Projection Correctness Observability
  - ADR-0083: Trace Budget and Fanout Summarization
  - ADR-0088: Native File Value Write Fast Path
  - ADR-0090: AuthZ Candidate Selection Index
  - `crates/temper-store-postgres/src/platform.rs`
  - `crates/temper-server/src/state/dispatch/effects.rs`
  - `crates/temper-server/src/query_projection_metrics.rs`

## Context

The latency program has moved the top measured bottleneck away from Cedar candidate scanning and toward projection/write fanout.
The latest production proof for PERF-001B (`authz-index-live-proof-20260516164954`) showed the exact File create trace at roughly 62.5 ms p50 / 65.0 ms p95, with `dispatch.phase.query_projection` around 20.8 ms p50 / 25.4 ms p95 and `entity.get_or_spawn_tenant_actor_with_fields` p95 around 41.9 ms.
Datadog also shows the File `$value` path still has meaningful PUT/read latency after the native fast path.

The current Postgres query projection write is intentionally simple:

1. Upsert the full `entity_catalog` row.
2. Delete every `entity_field_index` row for `(tenant, entity_type, entity_id)`.
3. Reinsert every scalar indexed field, one SQL statement per field.

That guarantees stale EAV rows do not survive, but it turns small updates into many tiny SQL writes. On live File proof traces this appears as repeated `entity_field_index` inserts, repeated delete/index update work, and `dispatch.phase.query_projection` spans that now dominate the in-process write path more than AuthZ candidate selection.

The query projection is a derived read model, so correctness is more important than raw write speed. Any optimization must keep:

- `entity_catalog.fields` as the complete projected JSON object for read-by-id and catalog-fast reads.
- `entity_field_index` as a scalar `$filter` pushdown index with no stale rows.
- tenant isolation and RLS behavior unchanged.
- replay parity, sampled shadow checks, and applied-sequence metrics meaningful.

## Decision

Use diffed `entity_field_index` maintenance for Postgres query projection upserts.
The write will still update `entity_catalog` on every accepted projection update, but it will stop deleting and reinserting unchanged field-index rows.

### Serialize Through the Catalog Row

Inside the existing transaction, upsert the new `entity_catalog` row first.
The catalog row is the per-entity projection serialization point: existing rows are locked by the `ON CONFLICT DO UPDATE`, and the first writer creates the row so concurrent first-writer transactions wait on the same primary key conflict before they inspect the field index.

**Why this approach**: Loading the catalog row with `FOR UPDATE` is not enough when the row does not exist yet. Upserting the catalog before diffing makes new and existing entity updates use one serialization point without adding a new lock table, cache, or advisory-lock protocol.

### Keep Catalog Writes Loud and Complete

Always write the new `entity_catalog` state before committing, including `fields`, `status`, `sequence_nr`, `projection_hash`, and `updated_at`.
Do not use this ADR to introduce projection decision caches or to skip catalog updates solely because projected fields are unchanged.

**Why this approach**: The catalog row carries authoritative projection fields and sequence metadata. Even when indexed fields are unchanged, the latest sequence number is correctness evidence for replay parity and shadow checks.

### Diff Scalar Index Rows

Load the actual `entity_field_index` rows for the entity with `FOR UPDATE` after the catalog upsert has serialized the transaction.
Convert the new projected JSON object into a bounded scalar index map using the same scalar extraction and max indexed-value-byte rule already used today.

Then apply:

- delete existing rows whose fields are absent from the new scalar map or no longer indexable;
- delete existing rows whose scalar indexed value changed;
- upsert rows that are new, changed, or whose denormalized status changed;
- preserve rows whose field value and status already match the new projection.

**Why this approach**: This preserves the no-stale-index-row invariant while turning common small updates into O(changed fields) writes instead of O(all indexed fields) writes.

### Preserve Fallback Simplicity

The first implementation should be Postgres-only and should not change Turso behavior.
Turso remains a separate storage surface with its own correctness tests.
The Postgres path can fall back to the current delete-all/reinsert-all algorithm when it cannot safely compute a diff.

**Why this approach**: Production uses Postgres for this measured bottleneck, and narrowing the first change reduces correctness risk.

### Measure Row Operations

Continue emitting the existing projection indexed/skipped field metrics.
At minimum, local tests must prove unchanged field-index rows are not rewritten while changed, removed, and unindexable fields are reconciled correctly.

**Why this approach**: This optimization is only valuable if it reduces actual write amplification. The observability program should be able to prove the row-operation reduction live, not infer it from code shape.

## Rollout Plan

1. **Phase 0 (Immediate)** — Implement Postgres diffed `entity_field_index` maintenance behind the existing projection upsert API. Add focused Postgres tests for unchanged fields, changed fields, removed fields, long-value skips, and catalog preservation.
2. **Phase 1 (Production proof)** — Roll into TemperPaw, deploy, rerun the File/OData proof, and compare Datadog `dispatch.phase.query_projection`, field-index SQL counts, projection update duration, and replay/shadow correctness.
3. **Phase 2 (Follow-up)** — If the diffed path helps but SQL count is still high, consider set-based `UNNEST` upserts/deletes and event-level coalescing for background projection work.

## Readiness Gates

- `cargo test -p temper-store-postgres` focused projection tests pass.
- `cargo check -p temper-store-postgres` passes.
- `cargo clippy -p temper-store-postgres --all-targets -- -D warnings` passes.
- `cargo test -p temper-server --test query_projection_backfill` passes replay/backfill coverage when server projection behavior changes.
- `cargo check -p temper-server` passes when server projection behavior changes.
- DST review is run for simulation-visible server changes if the implementation touches `temper-server`.
- Live proof preserves read-after-write correctness and Datadog shows no replay/shadow drift increase.

## Consequences

### Positive

- Small updates stop rewriting every indexed field.
- Projection spans should shrink on entity updates and File version-recording paths.
- The read model remains fully durable and queryable; no cache invalidation problem is introduced.

### Negative

- The Postgres projection path becomes more complex.
- Each upsert now reads the current field-index rows after writing the catalog row, so no-change or small-change updates improve while cold creates still pay the full insert cost.
- Status changes require care because `status` is denormalized into every EAV field row.

### Risks

- A diff bug could leave stale field-index rows. Mitigation: delete changed/removed rows before upserting replacements, test replay parity, and keep fallback full rebuild available.
- Serializing through the catalog upsert could add contention under concurrent updates to the same entity. Mitigation: the row is already the projection serialization point; measure transaction duration and wait evidence before expanding scope.
- A long field that crosses the indexable-size boundary could be mishandled. Mitigation: reuse the current scalar extraction and max-byte rule for both old and new maps.

### DST Compliance

- The primary change is in `temper-store-postgres`, outside simulation-visible crates.
- If `temper-server` metrics or dispatch tests are touched, do not introduce wall-clock or random behavior into simulation-visible logic. Existing production-only metrics using `Instant` must keep `// determinism-ok` annotations.

## Non-Goals

- No change to Cedar authorization or governance semantics.
- No decision cache, projection cache, or out-of-process read-model service.
- No Turso rewrite in the first PR.
- No presigned/direct blob upload architecture in this ADR.
- No change to OData `$filter` semantics.

## Alternatives Considered

1. **Keep full delete/reinsert** — Correct and simple, but live evidence shows it is now a real latency source.
2. **Move all filters to JSONB GIN and delete `entity_field_index`** — Potentially cleaner long-term, but it would require a broader OData SQL translator migration and new query-plan proof.
3. **Cache projected fields in memory** — Faster for repeated updates, but invalidation and multi-instance correctness would be harder than the current durable source-of-truth comparison.
4. **Batch with `UNNEST` only** — Reduces round trips but still rewrites unchanged fields. This remains a possible Phase 2 after diffing.

## Rollback Policy

Revert to the current full rebuild algorithm for Postgres projection upserts.
Because `entity_catalog.fields` remains the complete source of truth, a rollback can repair any suspected stale field index by running the existing projection backfill/replay path.
