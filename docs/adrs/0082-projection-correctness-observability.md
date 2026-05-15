# ADR-0082: Projection Correctness Observability

- Status: Proposed
- Date: 2026-05-14
- Deciders: Temper core maintainers
- Related:
  - ADR-0058: Query-Plane Hot-Field Opt-Out and Stable Projections
  - ADR-0077: Catalog-first OData entity materialization
  - ADR-0081: Latency and Observability Acceleration Program
  - `crates/temper-server/src/query_projection_metrics.rs`
  - `crates/temper-server/src/state/dispatch/effects.rs`
  - `crates/temper-server/src/state/entity_ops.rs`
  - `crates/temper-server/src/state/projection_backfill.rs`
  - `crates/temper-store-postgres/`
  - `crates/temper-store-turso/`

## Context

Temper's fast OData path depends on the durable query projection
(`entity_catalog` and `entity_field_index`). ADR-0077 made catalog-first reads
the intended fast path, and ADR-0058 reduced needless index churn with
`query_indexed = false` and projection hashes.

The latency program identified projection writes as a major optimization target:
per-field index write amplification can produce huge traces and expensive write
bursts. However, projection performance cannot be optimized safely unless the
runtime can also prove projection correctness. Today Temper has useful but
incomplete signals:

- background projection update enqueue/error/duration metrics exist;
- projection backfill records snapshot misses only;
- projection update failures are logged;
- `entity_catalog.sequence_nr` exists but is not surfaced as a correctness
  signal;
- there is no clear runtime view of projection update queue delay, last applied
  sequence, backfill replay coverage, deleted-row cleanup, or drift/parity
  status.

That gap makes it hard to distinguish "projection is fast" from "projection is
missing, stale, or silently drifting." The first OBS-003 slice must repair
measurement before any projection write-amplification optimization.

## Decision

### Sub-Decision 1: Projection metrics report freshness and source

Projection update metrics will distinguish the source of a projection write:

- `background_dispatch` for off-critical-path dispatch projection updates;
- `create`, `field_update`, and `delete` for synchronous entity operations that
  intentionally gate the user-visible acknowledgement;
- `backfill_snapshot` and `backfill_replay` for startup/recovery projection
  repair.

Metrics remain low-cardinality: tenant, entity type, operation, source, and
result are allowed; entity ID is never a metric tag.

**Why this approach**: source tells operators whether latency or errors are
affecting live dispatch, synchronous write acknowledgements, or recovery
repair. Avoiding entity IDs keeps Datadog usable at production scale.

### Sub-Decision 2: Projection sequence is a first-class signal

Every successful projection upsert/removal path should record the latest
authoritative sequence number applied to the projection, tagged by tenant,
entity type, operation, and source.

**Why this approach**: `sequence_nr` is the bridge between the authoritative
event stream and the derived query plane. Surfacing it is the minimum signal
needed before adding lag, drift, and parity checks.

### Sub-Decision 3: Background projection queue delay is measured separately

Background dispatch projection updates should record:

- time spent waiting for the spawned projection task to start;
- end-to-end time from enqueue to projection completion;
- result and applied sequence.

**Why this approach**: a slow projection can be SQL work, pool wait, task
scheduling delay, or errors/retries. Separating queue delay from store duration
prevents the latency program from optimizing the wrong layer.

### Sub-Decision 4: Backfill coverage is observable

Startup/recovery projection backfill should emit counts and duration for:

- entities considered;
- snapshot-projected entities;
- replay-projected entities;
- deleted entities cleaned from the projection;
- replay event counts;
- errors and missing transition tables.

**Why this approach**: projection backfill is the correctness repair path after
migration, cold start, or previous projection failures. It needs its own
coverage signal instead of relying on logs.

### Sub-Decision 5: Drift/parity checks are an optimization gate

This ADR's first implementation slice records the sequence and coverage signals
needed to make drift checks meaningful. Catalog-fast OData reads may also opt
into deterministic sampled shadow reads via
`TEMPER_ODATA_CATALOG_SHADOW_READ_EVERY`, comparing catalog-derived status,
projected fields, and sequence number with authoritative actor state in a
background task. Replay parity checks compare active projection rows with
authoritative event-log replay outside the request path and emit dedicated
parity drift/error metrics.

**Why this approach**: always-on shadow reads can be expensive and should be
sampled/budgeted deliberately. The first slice creates the telemetry substrate
without adding read-path cost.

### Sub-Decision 6: Durable catalogs must preserve full projected fields

Projection correctness depends on comparing the same value shape that OData
fast reads return. Postgres already stores projected `fields` in
`entity_catalog`; Turso must do the same instead of reconstructing catalog rows
from the scalar EAV filter index. The EAV index remains useful for filter
pushdown, but it is not the authoritative projected read body.

**Why this approach**: reconstructing catalog fields from scalar filter rows
erases JSON type and shape information, which can make fast reads disagree with
actor reads even when scalar filters keep working.

## Rollout Plan

1. **Phase 0 (Immediate)** - Add low-cardinality projection freshness,
   sequence, queue-delay, and backfill coverage metrics. Update the living
   latency dashboard with metric names and verification status.
2. **Phase 1 (Immediate follow-up)** - Add sampled projection drift checks for
   catalog-fast reads, disabled by default and enabled via a deterministic
   sampling interval.
3. **Phase 2 (Immediate follow-up)** - Add Datadog dashboard and monitor
   definitions for queue delay, end-to-end update latency, applied sequence,
   backfill coverage, shadow drift, and shadow sequence gap.
4. **Phase 3 (Immediate follow-up)** - Add replay parity verification metrics
   and tests with explicit budgets.
5. **Phase 4 (Optimization gate)** - Use the new signals to verify projection
   correctness before implementing diffing, batching, coalescing, or selective
   indexing changes.

## Readiness Gates

- Metrics compile and have focused unit coverage.
- `cargo check -p temper-cli` passes.
- The living dashboard records the new metric surface and remaining live
  Datadog proof.
- TemperPaw Datadog dashboard and monitor config contains the new metric
  surface before deployment.
- Sampled shadow reads are disabled by default and deterministic when enabled.
- Durable query-plane catalogs preserve full projected fields JSON separately
  from scalar filter indexes.
- Before projection performance changes, Datadog must show projection update
  latency, queue delay, backfill coverage, applied sequence, and drift/parity
  checks for the relevant deployment.

## Consequences

### Positive

- Projection speed work gains a correctness signal instead of relying on hope
  and logs.
- Operators can separate background lag from synchronous projection failures.
- Backfill repair becomes measurable.

### Negative

- More metrics add dashboard and monitor work.
- Sequence gauges do not prove absence of drift by themselves; they are a
  prerequisite signal, not the final proof.

### Risks

- Tenant/entity-type tags can still grow if a deployment has many tenants or
  generated entity types. Mitigation: keep tags limited to existing operational
  dimensions and avoid entity IDs.
- Background task queue delay uses wall-clock measurement in server production
  code. Mitigation: use the same metrics-only wall-clock pattern already used
  in dispatch and runtime metrics, and keep it outside deterministic
  simulation logic.

### DST Compliance

This decision touches `temper-server`, a simulation-visible crate. The metrics
use wall-clock `Instant` only for production observability timing and do not
feed actor state, transition decisions, persistence contents, or simulation
control flow. Any added wall-clock call must be annotated where needed with
`// determinism-ok`.

## Non-Goals

- Replacing the durable query projection.
- Making catalog-first reads default for all entity shapes.
- Accepting projection drift as a performance tradeoff.
- Adding entity ID tags to metrics.
- Implementing projection write diffing or batching in the same slice.

## Alternatives Considered

1. **Optimize projection writes first** - Rejected because faster derived writes
   are unsafe without lag/drift/replay evidence.
2. **Rely on DBM query plans only** - Rejected because DBM explains database
   work but cannot prove projection freshness or parity against the event log.
3. **Always shadow-read every catalog response** - Deferred because it could
   erase the latency win from catalog-first reads. Use sampled/budgeted parity
   checks instead.

## Rollback Policy

Remove the new metric emissions and dashboard panels. No database migration or
state migration is required for the initial observability-only slice.
