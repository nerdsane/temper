# ADR-0096: Set-Based Projection Index Reconciliation

- Status: Proposed
- Date: 2026-05-17
- Deciders: Temper core maintainers
- Related:
  - ADR-0082: Projection Correctness Observability
  - ADR-0091: Query Projection Diff Index Upserts
  - ADR-0095: Projection Transaction Fast Path
  - `crates/temper-store-postgres/src/platform.rs`
  - `crates/temper-store-postgres/src/store_projection_test.rs`

## Context

ADR-0095 shortened the Postgres query-projection transaction by moving scalar
index extraction before `BEGIN` and by skipping field-index reconciliation when
the catalog `status` and `projection_hash` are unchanged. Production evidence
from the first `c16e0201c1490e0496f4964f5c72b704bb8cd216` rollout confirms the
fast path is correct and useful, but also shows the next measured residual:

- `temper_postgres_pool_acquire_duration_ms` for `query_projection_upsert`
  stays low, roughly 1.4-10.1 ms in the current window.
- `temper_postgres_transaction_duration_ms` for `query_projection_upsert`
  still reaches roughly 385.6 ms in the same window.
- `temper_query_projection_update_end_to_end_duration_ms` for
  `Session/background_dispatch` still reaches roughly 386.2 ms, while File and
  FileVersion background projection updates are much lower.
- `temper_postgres_projection_index_reconciliations_total` shows changed
  `diff` reconciliation dominating the window: about 408 diff paths versus 64
  inserts and 26 skipped-unchanged paths.
- The average indexed-field count during the hot bins is around 25-28 scalar
  fields per projection update.

The remaining cost is therefore not mainly pool wait and not mainly unchanged
projection no-ops. It is changed projection reconciliation performing one
`DELETE` or `INSERT ... ON CONFLICT` per field while the per-entity catalog row
is locked.

## Decision

Keep ADR-0091 and ADR-0095 correctness semantics, but make changed
field-index reconciliation set-based in Postgres.

### Replace Per-Field Deletes With One Anti-Join Delete

For changed projections, delete stale `entity_field_index` rows with one SQL
statement. The statement compares existing rows against the incoming scalar
index supplied as `text[]` arrays via `unnest`.

Rows are deleted when the incoming projection no longer has the field, when the
field value changed, or when the denormalized status changed.

**Why this approach**: The delete condition is the same as ADR-0091's row-by-row
diff, but the database can evaluate it in one statement while holding only the
row locks it needs.

### Replace Per-Field Upserts With One Batched Upsert

For changed projections, insert or update all incoming scalar index rows with
one `INSERT ... SELECT FROM unnest(...) ON CONFLICT DO UPDATE` statement.

The conflict update includes a `WHERE` clause so an unchanged row is not
rewritten:

- update when `field_value IS DISTINCT FROM EXCLUDED.field_value`;
- update when `status IS DISTINCT FROM EXCLUDED.status`;
- otherwise keep the existing row untouched.

**Why this approach**: This preserves the "unchanged rows are not rewritten"
contract tested by `xmin`, while replacing N SQL round trips with one statement.

### Preserve Catalog Serialization

The existing catalog-row lock from ADR-0095 remains the serialization point.
Set-based index reconciliation runs only after the catalog row is inserted or
locked and updated.

**Why this approach**: The field index is derived from the catalog projection.
Serializing per entity through `entity_catalog` keeps concurrent projection
updates ordered without requiring an application-side cache or broad table lock.

### Keep Existing Reconciliation Path Metrics

Keep the existing `insert`, `diff`, and `skipped_unchanged` path labels.

**Why this approach**: The metric's question remains "which logical
reconciliation path did this update take?" The SQL implementation detail
changes from row-by-row to set-based, but the rollout should remain comparable
with the ADR-0095 proof.

## Rollout Plan

1. **Phase 0 (Immediate)** - Implement Postgres set-based field-index
   reconciliation behind the existing `upsert_query_projection` API. Add tests
   proving unchanged rows are not rewritten, removed fields are deleted, status
   changes update all rows, and empty scalar indexes clear stale index rows.
2. **Phase 1 (Production proof)** - Roll into TemperPaw, deploy, and compare
   current-version `Session/background_dispatch` p95, `query_projection_upsert`
   transaction p95, reconciliation path mix, and projection correctness signals.
3. **Phase 2 (Follow-up)** - If changed projection p95 remains high, inspect
   the Session projected-field shape and evaluate projection coalescing or
   entity-specific hot-field opt-outs that do not weaken OData expectations.

## Readiness Gates

- Focused `temper-store-postgres` projection tests pass.
- `cargo check -p temper-store-postgres` passes.
- `cargo clippy -p temper-store-postgres --all-targets -- -D warnings` passes.
- `cargo fmt --all -- --check` and `git diff --check` pass.
- Production proof shows no projection drift, no read-after-write regression,
  and explainable movement in `Session/background_dispatch` projection p95.

## Consequences

### Positive

- Changed projections use two set-based SQL statements for field-index
  maintenance instead of one statement per changed field.
- The catalog row lock is held for less client/server round-trip time.
- Existing correctness and replay-parity rules stay intact.

### Negative

- The SQL is denser than the row-by-row diff and depends on Postgres array
  binding.
- DBM samples will attribute more work to fewer statements, so the rollout must
  read both transaction p95 and statement-level DBM samples.

### Risks

- Empty scalar-index projections could fail to clear stale field rows if the
  `unnest` anti-join is wrong. Mitigation: add an explicit test for scalar fields
  changing to only object/array/null values.
- A status-only change could leave rows with old denormalized status if the
  conflict-update `WHERE` clause is incomplete. Mitigation: include status in
  both delete and upsert distinctness checks.
- This may expose a different bottleneck, such as catalog JSONB writes or
  downstream actor work. Mitigation: keep the Datadog proof grouped by entity,
  source, reconciliation path, and transaction phase.

### DST Compliance

This change is confined to `temper-store-postgres`, which is not a
simulation-visible crate. It introduces no wall-clock, random, filesystem, or
network behavior beyond existing production database operations and metrics.

## Non-Goals

- No change to OData semantics.
- No change to projection replay parity or drift detection.
- No in-memory projection cache.
- No Session-specific denormalization or hidden field opt-out in this slice.
- No Turso projection rewrite.

## Alternatives Considered

1. **Keep row-by-row diffing** - Correct but leaves the measured changed
   projection path paying per-field SQL round trips.
2. **Delete all rows and bulk insert all rows** - Simpler SQL, but regresses the
   ADR-0091 guarantee that unchanged field-index rows are not rewritten.
3. **Entity-specific Session projection trimming first** - May be useful later,
   but it changes product/read semantics. The measured SQL shape can be improved
   without narrowing what projections expose.
4. **Projection coalescing before SQL optimization** - Promising for bursts, but
   introduces ordering and freshness tradeoffs. The set-based SQL change keeps
   the same one-update-in, one-projection-write-out semantics.

## Rollback Policy

Revert `upsert_query_projection` to the ADR-0095 row-by-row reconciliation
implementation. Because `entity_catalog.fields` remains complete and
authoritative, any suspected field-index issue can be repaired by projection
replay/backfill.
