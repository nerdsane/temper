# ADR-0148: Bound Derived Writes Off The Dispatch Hot Path

- Status: Proposed
- Date: 2026-06-18
- Deciders: Temper core maintainers
- Supersedes: ADR-0142
- Related:
  - ADR-0067: Trajectory outbox
  - ADR-0091: Query projection diff index upserts
  - ADR-0095: Projection transaction fast path
  - ADR-0134: Query plane read contract
  - ADR-0135: Authorized query plane pages
  - `crates/temper-server/src/entity_actor/actor.rs`
  - `crates/temper-server/src/state/dispatch/effects.rs`
  - `crates/temper-server/src/state/dispatch/composite.rs`
  - `crates/temper-store-postgres/src/platform.rs`
  - `crates/temper-store-postgres/src/store.rs`

## Context

Canonical Foresight runs on the `foresight` deployment backed by the Supabase
pooler. Datadog evidence shows DB time dominates run latency. The hottest
Temper-core buckets are derived writes and broad reads, not the event journal
append itself: `snapshots`, `snapshot_history`, `entity_catalog`,
`entity_field_index`, OTS trajectory/session artifact persistence, and Session
OData reads that can materialize far more rows than the caller needs.

ADR-0142 moved dispatch projection writes inline to provide read-your-writes
for OData point reads. That fixed a correctness bug, but it also put
`entity_catalog` and `entity_field_index` writes on every successful dispatch.
Foresight dispatches many Session-related transitions in tight loops, so this
turns one synchronous journal append into several synchronous DB operations.

Snapshots have the same shape. The event journal must stay synchronous because
it is the durable commit record. Snapshot rows are replay accelerators and
segment boundaries. Waiting for both latest-snapshot and history writes in the
entity actor path increases tail latency and couples dispatch success to a
derived checkpoint that can safely lag when lag is measured and bounded.

## Decision

Dispatch commits continue to synchronously append the event journal. Derived
persistence is moved behind bounded, observable queues:

### Sub-Decision 1: Snapshots Are Enqueued After Journal Commit

Entity actors serialize a snapshot at the configured interval, enqueue it to a
bounded snapshot writer, and advance their in-memory replay budget only after
the enqueue succeeds. The writer coalesces by persistence ID and sequence so an
older pending snapshot is skipped before a database connection is acquired.

When the queue is full or enqueue fails, the actor keeps its unsnapshotted tail
count. It may eventually reject new transitions at the existing
`MAX_EVENTS_SINCE_SNAPSHOT` budget rather than pretending a durable checkpoint
exists.

**Why this approach**: snapshots are optimization state, but the replay budget
is a safety invariant. Enqueue-success semantics keep the actor fast while
preserving a bounded replay tail if the writer falls behind.

### Sub-Decision 2: Query Projection Writes Are Bounded And Coalesced

Single-dispatch and composite-dispatch query projection updates enqueue durable
work instead of awaiting `entity_catalog`/`entity_field_index` writes inline.
The queue coalesces by `(tenant, entity_type, entity_id)` and keeps only the
highest sequence number. Stale work is skipped before acquiring a database
connection.

Read-your-writes is preserved by response semantics and read planning:

- the dispatch response remains authoritative for the write it just committed;
- point reads that require immediate consistency can fall back to the actor
  when the projection sequence is behind the requested sequence;
- collection reads use projection rows only for bounded, indexed pages and
  surface projection lag through telemetry.

**Why this approach**: ADR-0142's correctness problem was an acknowledged write
being served stale by a projection-only read. The fix is to make reads aware of
projection lag, not to make every dispatch wait for all derived indexes.

### Sub-Decision 3: Session Collection Reads Must Push Down Bounds

Supported Session/OData queries used by Foresight and DSF2 must use the query
plane or a Temper-native read model with SQL-side tenant/type/filter/page
limits. They must not materialize the full Session entity set to answer a
bounded page, filtered lookup, or latest-session style query.

**Why this approach**: moving writes off the hot path is not enough if a
follow-up read immediately asks Postgres and the actor layer to hydrate every
Session row.

### Sub-Decision 4: OTS Uploads Use Durable Admission And Retryable Status

Full OTS trajectory POST persistence and terminal Session trajectory emission
effects are not dispatch-critical. The POST handler durably records the
artifact by `trajectory_id` with a persistence status before acknowledging the
upload. This admission write includes the full trajectory JSON artifact, so the
tradeoff is explicit: OTS POST latency pays one durable write to avoid lossy
terminal artifacts, while Session dispatch does not wait for the POST handler.
A bounded in-memory worker drains accepted work and advances the durable status
from `queued` to `persisted` or `failed`; restart recovery can replay rows
still marked `queued`. MCP clients treat `503` and transient transport errors
as retryable with bounded backoff, including final trajectory uploads on
session close.

**Why this approach**: Foresight needs reliable trajectory artifacts, but the
terminal Session transition should not be coupled to a best-effort in-memory
side effect. The durable admission write is the reliability boundary; the
status transition and any retry/failure accounting happen outside dispatch.

## Rollout Plan

1. **Immediate PR** -- introduce bounded coalescing writers for snapshots and
   query projection updates, route single and composite dispatch through them,
   add lag/error metrics, and add tests proving dispatch no longer awaits the
   derived writes.
2. **Same effort** -- add Session query tests for the Foresight/DSF2 query
   shapes and make the query plane satisfy them without full table
   materialization.
3. **Same effort** -- give OTS trajectory uploads durable admission,
   retryable MCP finalization, and status telemetry so terminal artifacts are
   not silently lost under transient saturation.

## Readiness Gates

- Event journal append remains synchronous and optimistic-concurrency guarded.
- Snapshot enqueue failure does not reset `events_since_snapshot`.
- Projection updates skip stale sequences before opening a DB write.
- Projection update failures are retried with bounded backoff, and stale
  retries are skipped if a newer update for the same entity has already been
  queued.
- Composite dispatch does not synchronously wait for query projection writes
  after the atomic journal batch commits.
- Supported Session OData queries are bounded in the backing store.
- Supported bounded Session OData page reads do not compute storage counts
  unless `$count=true` was requested.
- OTS upload admission either returns success after a durable queued row exists
  or returns a retryable error to the caller.
- MCP final trajectory upload retries `503` and transient transport failures
  with a bounded backoff.
- Datadog re-measurement uses `@version:<deployed-version>` and
  the active database peer/service tag for the deployed target (`Postgres-v79y`
  currently reports as `foresight-railway-postgres`).

## Consequences

### Positive

- Dispatch latency is no longer multiplied by snapshot, projection index, and
  trajectory artifact writes.
- Derived DB writes can batch and coalesce naturally under bursty Foresight
  traffic.
- Projection and snapshot lag becomes visible as a product signal instead of a
  hidden tail-latency tax.

### Negative

- Some reads must reason about projection freshness instead of assuming the
  projection is always current at dispatch acknowledgement time.
- Keyed and bounded-candidate OData reads can fall back to actor materialization
  and repair missing catalog rows; pure collection membership remains eventual
  until the projection queue catches up.
- A saturated derived-write queue can delay OData collection freshness, though
  the journal remains authoritative.
- OTS POST acknowledgements now mean durable admission, not necessarily that
  downstream status advancement has completed.
- OTS POST latency still includes one full-artifact durable write; this is a
  reliability tradeoff for terminal trajectory artifacts, not a dispatch-path
  write reduction by itself.

### Risks

- If lag metrics are ignored, stale projections could persist longer than
  acceptable. Mitigation: emit per-queue depth, lag, skipped-stale, error, and
  applied-sequence metrics.
- If a read path cannot tolerate projection lag and lacks actor fallback, it
  can observe stale data. Mitigation: add sequence-aware read tests for the
  Foresight/DSF2 Session paths touched by this work.
- If the durable OTS admission write itself is slow, the POST caller still
  pays that cost. Mitigation: keep this out of the Session dispatch path,
  retry transient failures at the MCP boundary, and measure OTS POST latency
  and outbox status/latency metrics separately from dispatch latency.

### DST Compliance

- Simulation-visible transition semantics remain unchanged: action evaluation,
  journal envelopes, and replay use the existing deterministic path.
- New wall-clock timing is metrics-only and annotated where necessary.
- Bounded queues are production side-effect infrastructure and do not affect
  deterministic simulation state.

## Non-Goals

- Switching Foresight away from Codex or changing model/provider behavior.
- Increasing broad run concurrency before the DB hot path is reduced.
- Moving app or agent logic from TemperPaw into the Temper kernel.
- Removing existing projection correctness repair or parity mechanisms.

## Alternatives Considered

1. **Keep ADR-0142 inline projection writes** -- preserves immediate projection
   freshness, but keeps `entity_catalog` and `entity_field_index` latency on
   every dispatch and does not address Foresight DB amplification.
2. **Disable projection writes during Foresight runs** -- fast but incorrect;
   OData and file/session read paths depend on the projection.
3. **Only tune PostgreSQL indexes** -- useful, but it does not reduce the
   number of writes or the fact that derived writes are awaited by dispatch.

## Rollback Policy

Set the derived-write mode back to inline for projection and synchronous for
snapshots, drain outstanding queues, and keep the new lag metrics in place to
confirm the rollback removes stale projection exposure.
