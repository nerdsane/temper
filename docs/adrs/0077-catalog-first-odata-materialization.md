# ADR-0077: Catalog-first OData entity materialization

- Status: Accepted
- Date: 2026-04-30
- Deciders: Temper core maintainers
- Related:
  - ADR-0076: Eliminate `ServerEventStore` enum (the abstraction that lets us add catalog-first reads without branching on backend)
  - ADR-0074: Turso → Postgres ETL methodology (revealed the projection-not-populated gap)
  - `crates/temper-server/src/odata/read_support.rs`
  - `crates/temper-server/src/storage/mod.rs`

## Context

The OData read path on `/tdata/{EntitySet}` materializes a list response by calling `state.get_tenant_entity_state(tenant, entity_type, id)` once per result row. Each call spawns or wakes the entity actor and replays its event stream from snapshot.

Production trace `9713e924e7c1e0428e2c82b5fbb1b215` (2026-04-30 18:23 UTC) on the openpaw deployment showed `GET /tdata/DesignLanguages` taking 22.14 seconds against 210 entities — a per-row median of ~700 ms with several rows above 2 s. The katagami.ai SSR awaits this call on every uncached homepage request.

The platform already maintains `entity_catalog`, a wide projection upserted on every transition (`crates/temper-server/src/storage/mod.rs:2102, 2170, 2240`). Each row contains `(tenant, entity_type, entity_id, status, fields JSONB, sequence_nr, ...)`. For a list view the projection holds exactly the data the OData response needs — but the read path does not query it.

This is the reason `entity_catalog` exists. The cost of per-row actor hydration is the cost of bypassing it.

## Decision

Add a catalog-first batch read to `materialize_entity_set_entities` in `read_support.rs`:

1. Define `EntityCatalogRow` (entity_id, status, fields, sequence_nr) in `crates/temper-server/src/storage/mod.rs` as a backend-neutral type.
2. Add `QueryPlaneStore::load_entity_catalog_rows(tenant, entity_type, entity_ids) -> Result<Option<Vec<EntityCatalogRow>>>` to the trait, with default impl `Ok(None)` so non-catalog backends opt out cleanly.
3. Implement on `PostgresEventStore` via `SELECT entity_id, status, fields, sequence_nr FROM entity_catalog WHERE tenant=$1 AND entity_type=$2 AND entity_id = ANY($3)`.
4. Implement on `TursoEventStore` with the equivalent `IN (?, ?, ...)` libsql query.
5. `TenantStoreRouter` delegates per tenant.
6. In `materialize_entity_set_entities`, when `TEMPER_ODATA_CATALOG_FAST_READ=1`:
   - Issue one batch catalog read for the page's IDs.
   - For each ID with a catalog hit, build the response body from the row directly.
   - For misses (catalog stale, never written, or entity has non-empty `counters`/`booleans`/`lists` not yet projected), fall back to the existing `state.get_tenant_entity_state` path on a per-id basis.

The body shape is identical to the actor's `EntityState` serialization (`status`, `fields`, `entity_type`, `entity_id`, `sequence_nr`, `total_event_count`, `item_count`, `counters`, `booleans`, `lists`, `events`) so `enrich_entity_response` and OData clients see no difference.

## Why opt-in

Default `TEMPER_ODATA_CATALOG_FAST_READ=false`. Reasoning:

- Entity types that store state in `counters`/`booleans`/`lists` (not just `fields`) would render as empty maps when read from the catalog, since the projection schema only persists `status` + `fields`. Most temper entities don't use those collections, but the platform doesn't statically know which do.
- A flag-controlled rollout lets operators flip the fast path on per-deployment after verifying the entities they expose don't depend on the unprojected collections.
- Future work: extend `entity_catalog` to include the full `EntityState` JSONB (or add a parallel `entity_state_full` projection) so the flag can default on and apply universally.

## Why catalog freshness is acceptable here

Projection upserts happen in the same write path that appends events (`storage/mod.rs::*upsert_query_projection`), inside the same actor turn. There is no async lag. A reader sees catalog state as fresh as the actor would have served it, modulo a single in-flight transition (which the actor path also sees mid-flight). For OData reads the difference is below the threshold callers can detect.

## Consequences

**Positive**:
- `GET /tdata/DesignLanguages` against the openpaw production fleet drops from 22s to one indexed Postgres query (~10–50 ms p99 expected).
- The actor system stays cold for read-heavy workloads — fewer spawns, less mailbox pressure, less snapshot/event replay traffic.
- Sets the precedent for moving more OData read paths (single-entity GET, navigation property expansion) to catalog reads in follow-ups.

**Negative**:
- Two divergent code paths (catalog vs actor) for materialization. Mitigated by the fallback being the *same* old code, only invoked on miss.
- Entity types using `counters`/`booleans`/`lists` need either flag-off or the projection extension. Documented in the env-var description.
- Adds a new SQL query per list response (one batch). Trivial cost compared to the 210-actor spawn it replaces.

## Migration

No data migration required for new deployments — the projection is populated on every transition since ADR-0076.

For deployments that were migrated from another backend (e.g., Turso → Postgres ETL): the ETL must populate `entity_catalog` directly from `snapshots`, since migrated snapshots never replay through the live transition path. The openpaw 2026-04-30 cutover discovered this gap and backfilled `entity_catalog` (1287 rows) and `entity_field_index` (11363 rows) post-hoc; the methodology is captured in ADR-0074.

## Alternatives considered

- **Vercel ISR + revalidate** on katagami: caches the SSR'd HTML at the edge for N seconds. Mitigation, not a root fix; first request after revalidation still pays the 22s cost. Composes with this ADR but does not replace it.
- **OData response caching at the openpaw layer** (e.g., `Cache-Control: s-maxage=60`): same shape — moves the cost to background revalidation but does not eliminate it.
- **Eager actor warmup** at boot: paid once at startup but doesn't survive passivation; cold-actor cost returns after `TEMPER_ACTOR_IDLE_TIMEOUT_SECS`. Doesn't scale to tenants with many entity types.
- **Replace `entity_catalog` JSONB with the full serialized `EntityState`**: cleaner but changes the projection write path's invariants. Deferred — the current decision keeps the schema unchanged and the fast-read opt-in.
