# ADR-0111: Full-state catalog fast reads

- Status: Accepted
- Date: 2026-05-20
- Deciders: Temper core maintainers
- Related:
  - ADR-0077: Catalog-first OData entity materialization
  - ADR-0082: Projection correctness observability
  - ADR-0110: Bounded catalog shadow read probes
  - `crates/temper-server/src/odata/read_support.rs`
  - `crates/temper-server/src/storage/mod.rs`
  - `crates/temper-store-postgres/src/platform.rs`
  - `crates/temper-store-turso/src/store/field_index.rs`

## Context

ADR-0077 introduced catalog-first OData materialization because hydrating one actor per list row was already the dominant read latency source. The initial rollout stayed opt-in because the catalog row only persisted `(status, fields, sequence_nr)`. That is enough for many field-centric entities, but it is not enough for a platform contract: entities can store state in `item_count`, `counters`, `booleans`, and `lists`, and the query projection can also omit fields marked `query_indexed=false`.

The latency program revalidated this bottleneck in production after the curation child-session overlap deployment. Datadog trace `7e60f4017e6e8db56069df73b6f07196` for `GET /tdata/Taxonomies` on version `363c8c0aee9546ef8b3dc2d34ba9d9df6de6bcbe` spent about 1.067 seconds materializing 411 rows. The entity id source query was about 25 ms, and the row catalog SQL was under 100 ms; the rest came from repeated `entity.get_tenant_entity_state` actor hydration spans. A second live trace, `7f0a26d346290935d2386d90c6cd4b9d`, reproduced the same shape at about 1.000 seconds.

That means the system is paying actor replay and mailbox costs on read-heavy catalog endpoints even when the projection plane already knows the target row set. We need to remove that cost without compromising Temper's mission: verified entities, full audit history, tenant isolation, deterministic simulation, and projection correctness.

## Decision

Extend `entity_catalog` with a nullable full-state projection payload and make the OData catalog materializer prefer it.

### Full-state catalog payload

`EntityCatalogRow` gains `state: Option<serde_json::Value>`. Postgres stores it as `JSONB`; Turso stores it as serialized JSON text. The payload is the serialized `EntityState` response shape with unbounded recent `events` removed. It preserves:

- `entity_type`
- `entity_id`
- `status`
- `item_count`
- `counters`
- `booleans`
- `lists`
- `fields`
- `total_event_count`
- `sequence_nr`

The existing `fields` column remains the query/index projection. It is still the source for field pushdown and lightweight filtering. The new `state` column is the response projection.

### Read behavior

When a catalog row has `state`, `catalog_row_to_entity_body` returns that state with canonical OData metadata applied. This keeps list, single-entity, navigation, and stream parent reads aligned with the actor `EntityState` serialization.

When a catalog row lacks `state`, the code keeps the existing synthesized legacy body as a compatibility path. Operators can therefore roll this change through databases before all rows have been rewritten. Shadow checks and live parity tests decide when a deployment can rely on the fast path broadly.

### Write behavior

Every projection upsert writes both:

- `fields`: the filtered query projection used for SQL indexes and predicates.
- `state`: the full response projection used for safe fast reads.

This includes normal transitions, data-only create/update fast paths, background dispatch projection updates, and projection backfill/replay code.

### Correctness guard

Catalog shadow checks compare the catalog row with actor state. With this ADR, rows containing `state` must also be checked for full projected body parity after removing volatile OData annotations and unbounded events. The trace/log metric should make state drift distinguishable from field/status/sequence drift.

## Rollout Plan

1. **Phase 0 (Temper PR)** - Add the nullable schema column, storage plumbing, read materializer support, tests, and the ADR.
2. **Phase 1 (TemperPaw rollout PR)** - Deploy a Temper image containing the new projection payload. Keep the existing `TEMPER_ODATA_CATALOG_FAST_READ` control and bounded shadow-read controls in place.
3. **Phase 2 (Live parity)** - Run live reads for high-cardinality sets such as `Taxonomies` and compare fast catalog responses with sampled actor shadow reads. Monitor `projection.catalog.shadow_check` metrics and trace tags for drift.
4. **Phase 3 (Performance proof)** - Capture before/after Datadog traces for `GET /tdata/Taxonomies`, including row counts, candidate counts, materialized counts, trace ids, and p50/p95 timing.
5. **Phase 4 (Default policy decision)** - Only after production parity evidence, decide whether to default catalog fast reads on for deployments that maintain full-state catalog rows.

## Readiness Gates

- Postgres and Turso migrations are idempotent.
- `cargo test` covers the full-state response materializer.
- Projection upserts write `state` on normal transition paths and backfill paths.
- Shadow checks can detect full-state drift without causing unbounded actor work.
- Production before/after traces show the actor hydration fanout collapsing for the target OData list route.

## Consequences

### Positive

- High-cardinality OData entity sets can read from one projection query instead of hundreds of actor hydrations.
- The catalog fast path becomes correct for entities that use counters, booleans, lists, item counts, or non-indexed response fields.
- The old `fields` projection remains small and query-oriented; response correctness no longer depends on query-index policy.
- Shadow checks become stronger because they can compare full response state, not only status and filtered fields.

### Negative

- `entity_catalog` rows become larger.
- Every projection upsert writes an additional JSON payload.
- Backends must keep two related projection payloads: query fields and response state.

### Risks

- Larger catalog rows could increase write amplification. Mitigation: strip `events` from the state payload and continue storing only the latest response projection.
- Existing rows will initially have `state = NULL`. Mitigation: preserve the legacy synthesized body fallback and use replay/backfill to fill state.
- A buggy serializer could introduce projection drift. Mitigation: reuse `EntityState` serialization, strengthen shadow checks, and keep actor fallback available during rollout.

### DST Compliance

This touches `temper-server`, a simulation-visible crate. The state payload is derived from the already deterministic `EntityState` generated during an actor turn. No wall-clock time, randomness, filesystem, network I/O, or unordered iteration is added. Existing production-only asynchronous shadow work remains bounded by ADR-0110 controls.

## Non-Goals

- This ADR does not remove the event log or actor source of truth.
- This ADR does not make `entity_catalog` the write authority for entity state.
- This ADR does not change OData filtering semantics.
- This ADR does not immediately default catalog fast reads on for every deployment.

## Alternatives Considered

1. **Enable the existing partial catalog fast-read globally** - Fast for field-only entities, but unsafe for entities with counters, booleans, lists, item counts, or non-indexed fields. Rejected because the latency program must preserve correctness.
2. **Actor warmup for hot entity sets** - Reduces the first read after deploy but keeps read latency tied to actor lifecycle and replay cost. Rejected as a cache workaround.
3. **Dedicated OData response cache** - Can hide repeated reads but risks stale or tenant-leaky cache behavior unless it duplicates projection correctness logic. Rejected as a second projection plane.
4. **Store only extra counters/booleans/lists columns** - Smaller than full state, but still easy to miss future response-shape fields. Rejected because the contract we need is the `EntityState` response shape.

## Rollback Policy

Disable `TEMPER_ODATA_CATALOG_FAST_READ` to return reads to actor hydration. The nullable `state` column can remain unused. If write amplification is unacceptable, stop writing `state` while keeping the migration in place, then back out the storage plumbing in a follow-up PR.
