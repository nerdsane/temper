# ADR-0095: Projection Transaction Fast Path

- Status: Proposed
- Date: 2026-05-17
- Deciders: Temper core maintainers
- Related:
  - ADR-0082: Projection Correctness Observability
  - ADR-0091: Query Projection Diff Index Upserts
  - ADR-0092: Bounded Background File Reactions
  - `crates/temper-store-postgres/src/platform.rs`
  - `crates/temper-store-postgres/src/store_projection_test.rs`

## Context

PERF-002 changed Postgres query-projection writes from delete-all/reinsert-all index maintenance to diffed index maintenance. That removed a major source of write amplification while preserving `entity_catalog` as the complete projected read model and `entity_field_index` as the scalar filter index.

The next live production window after PERF-005D shows a different residual:

- `temper_postgres_pool_acquire_duration_ms` p95 is only about 4.8-5.0 ms for `query_projection_upsert`.
- `temper_postgres_transaction_duration_ms` p95 is about 429-443 ms for `query_projection_upsert`.
- `temper_query_projection_update_end_to_end_duration_ms` p95 reaches about 471-478 ms for `Session` background dispatch and about 217 ms for `File` background dispatch.
- Native blob transport p95 remains material, but is lower than the projection transaction tail in this window.

That means the next measured bottleneck is not broad pool starvation. It is the shape and amount of work performed while a projection transaction is open.

The current Postgres projection upsert still does these expensive things in the transaction:

1. Write the catalog row.
2. Read and lock all existing field-index rows for the entity.
3. Convert the new JSON projection into the scalar field index.
4. Diff every existing field against the new map.
5. Delete and upsert changed rows.

This is correct, but it still holds a database transaction open while doing CPU work and SQL work that can often be skipped. In particular, many background projection updates advance an entity sequence while leaving projected fields and status unchanged. Those updates still need `entity_catalog.sequence_nr` for replay parity and applied-sequence correctness, but they do not need to touch `entity_field_index`.

## Decision

Keep the projection correctness model from ADR-0091, but shorten the hot transaction path.

### Precompute the New Scalar Index Before Begin

Compute `scalar_index_fields(fields)` before acquiring the Postgres connection and before `BEGIN`.

**Why this approach**: The scalar index is pure, deterministic CPU work over the incoming projected JSON. It does not need a database lock. Moving it outside the transaction reduces lock hold time without changing the index contents.

### Lock the Existing Catalog Row First

When the catalog row exists, load `status` and `projection_hash` with `FOR UPDATE` before writing the new row. The locked row becomes the serialization point for the projection update.

When the catalog row does not exist, insert it. If a concurrent first writer wins the insert race, retry by locking the now-existing catalog row and follow the existing-row path.

**Why this approach**: ADR-0091 correctly noted that `SELECT ... FOR UPDATE` alone does not serialize first writers when no row exists. The insert-or-retry path keeps that safety property while making the common existing-row path able to compare the previous projection before rewriting it.

### Skip Field-Index Reconciliation on Projection No-Ops

If the previous catalog `status` and `projection_hash` match the incoming `status` and hash, update only the catalog row fields that must advance, especially `sequence_nr` and `updated_at`, then commit. Do not read, lock, diff, delete, or upsert `entity_field_index` rows in this no-op case.

**Why this approach**: The field-index rows are a function of projected fields plus denormalized status. If both the projection hash and status are unchanged, the scalar filter index is unchanged. Advancing the catalog sequence preserves correctness evidence for replay parity without paying index-maintenance cost.

### Preserve Full Reconciliation When Projection Changes

If status or projection hash changed, perform the ADR-0091 diffed reconciliation:

- lock actual `entity_field_index` rows for the entity;
- delete removed, unindexable, or changed fields;
- upsert new, changed, or status-changed fields;
- preserve unchanged rows.

**Why this approach**: The no-op fast path must not weaken stale-index protection. Any actual projected-field or status change still follows the proven diff algorithm.

### Emit Reconciliation Path Metrics

Emit a low-cardinality counter for the projection index path: `insert`, `diff`, or `skipped_unchanged`.

**Why this approach**: Datadog p95s can show whether the transaction tail improved, but the rollout also needs to prove whether live traffic is actually taking the no-op fast path. A path counter gives that proof without tagging entity IDs or field names.

## Rollout Plan

1. **Phase 0 (Immediate)** - Implement the Postgres-only fast path in `upsert_query_projection`, with tests for unchanged projection sequence advancement, unchanged index-row preservation, changed-field reconciliation, long-field removal, and concurrent first-writer safety where practical. Emit reconciliation path metrics for rollout proof.
2. **Phase 1 (Production proof)** - Roll into TemperPaw, deploy, rerun File and OData proof flows, and compare current-version `query_projection_upsert` transaction p95 plus projection end-to-end p95 by entity/source.
3. **Phase 2 (Follow-up)** - If projection tails remain high, evaluate set-based `UNNEST` index reconciliation and event-level projection coalescing for background dispatch.

## Readiness Gates

- Focused `temper-store-postgres` projection tests pass.
- `cargo check -p temper-store-postgres` passes.
- `cargo clippy -p temper-store-postgres --all-targets -- -D warnings` passes.
- `cargo fmt --all -- --check` and `git diff --check` pass.
- Production proof shows no projection drift, no read-after-write regression, and lower or explainably unchanged `query_projection_upsert` transaction p95.

## Consequences

### Positive

- No-op projection updates advance catalog sequence without locking or diffing field-index rows.
- Database transaction hold time shrinks because scalar-index extraction moves outside `BEGIN`.
- Correctness remains durable and replayable; no cache is introduced.

### Negative

- `upsert_query_projection` becomes a little more branched because it distinguishes insert, existing changed, existing unchanged, and first-writer race paths.
- Some cold-create work stays the same because new entities still need all index rows inserted.

### Risks

- A race in the first-writer path could skip needed field-index insertion. Mitigation: use insert-or-retry semantics, keep all field-index insertion in the successful insert path, and preserve tests around indexed-row presence.
- A false no-op classification could leave stale index rows. Mitigation: classify no-op only when both previous `projection_hash` and previous `status` match the incoming values.
- The optimization may not help if the p95 comes from changed large projections rather than no-op background updates. Mitigation: production proof must compare Datadog `entity_type`/`source` groups after rollout.

### DST Compliance

This change is confined to `temper-store-postgres`, which is not a simulation-visible crate. It introduces no wall-clock or random behavior beyond the existing production metrics timers.

## Non-Goals

- No change to OData semantics.
- No change to projection replay parity rules.
- No in-memory projection cache.
- No Turso rewrite in this slice.
- No event append sequence-table redesign in this first PERF-003 patch.

## Alternatives Considered

1. **Only move scalar extraction before `BEGIN`** - Safe but leaves no-op updates doing unnecessary row locks and diffing.
2. **Skip catalog writes for unchanged projections** - Faster but loses applied-sequence evidence needed for replay parity and correctness dashboards.
3. **Set-based `UNNEST` reconciliation immediately** - Promising for changed large projections, but it is a broader SQL rewrite. The no-op fast path is smaller and directly targets the measured transaction tail.
4. **Add a projection cache** - Would complicate invalidation and multi-instance correctness; the current bottleneck can be attacked without cache semantics.

## Rollback Policy

Revert `upsert_query_projection` to the ADR-0091 diffed-index implementation. Because `entity_catalog.fields` stays complete and authoritative, any suspected field-index issue can be repaired by projection replay/backfill.
