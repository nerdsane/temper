# ADR-0192: Exact Declared-Key and Durable-Projection Reconciliation

- Status: Proposed
- Date: 2026-07-13
- Deciders: Temper core maintainers
- Related:
  - ADR-0077: Catalog-only migration compatibility
  - ADR-0153: Declared composite-key index
  - ADR-0155: Declared vector access path
  - ARN-238: Stale declared-key ownership after delete or key removal
  - `crates/temper-runtime/src/persistence/`
  - `crates/temper-server/src/entity_actor/`
  - `crates/temper-server/src/odata/query_plane_read/`
  - `crates/temper-server/src/state/projection_backfill/`
  - `crates/temper-store-{postgres,sim,turso,redis}`

## Context

ADR-0153 co-committed declared-key rows with journal appends, but the persistence
contract could not distinguish “this caller does not maintain keys” from “this
keyed entity now owns the empty set.” Stores removed prior rows only for key names
present in the new set. A deleted entity, an all-null key, a removed declaration,
or a non-scalar key could therefore retain ownership indefinitely.

Repair also treated durable state as a single sequence. That is insufficient for
the data already supported by Temper:

- a snapshot-only migration generation and its first journal generation can use
  the same numeric sequence;
- a snapshot can be replaced at the same sequence with different bytes;
- a numerically newer stale snapshot or catalog row can coexist with an older
  terminal journal tombstone;
- a journal can be longer than the in-memory replay-tail budget;
- a projection writer can arrive after a source repair and overwrite it;
- a crash can land between an authoritative source write and its asynchronous
  catalog/EAV projection.

Those cases make key ownership, query projections, and actor recovery one source-
authority problem. Fixing only the key delete would allow a stale projection or
snapshot to reintroduce the same invalid state on the next repair.

## Decision

### 1. Declared keys use explicit exact-set ownership

`IndexReconciliation` carries an explicit `keys` signal. Participating callers
pass the entity's complete post-write `key_rows`, including an empty set. Plain
`append` does not reconcile keys; `append_with_keys` and keyed actor/composite
writes do. `PersistenceAppend` carries the same `reconcile_keys` decision for
atomic multi-stream writes.

Every durable write surface derives the final declared-key set: normal actions,
PATCH/PUT field updates, data-only creates, File initial-content writes, and
composite batches. A deleted entity and an all-null or otherwise unkeyable entity
participate with an authoritative empty set. `known_new` remains only a planning
hint; stores still validate the durable stream and key contract.

PostgreSQL validates claims, appends the journal records, deletes every prior row
for the participating entity, inserts the complete new set, and commits once. A
batch validates all streams and claims before publishing any deletion, permitting
an atomic ownership transfer. Sim constructs and validates the complete next key
map under its deterministic lock before mutating journals or ownership. Any
conflict rolls back the whole operation.

Turso remains non-authoritative for declared keys under ADR-0153 because it cannot
co-commit uniqueness with the journal. The capability boundary prevents its legacy
rows or watermarks from becoming authoritative hits or misses.

**Why this approach**: row presence cannot encode an authoritative empty set.
Deleting by entity identity also removes rows for declarations that no longer
exist, without sentinel values or store knowledge of the spec.

### 2. Actor recovery captures a complete durable source generation

Recovery captures all of the following before replay and verifies the same values
again before returning:

- the journal boundary, including its inclusive high-water and first terminal
  sequence;
- snapshot presence, sequence, and exact serialized bytes;
- the transition table used to replay the state.

Journal records are read in bounded ascending pages through the captured high-
water. Every page must contain the exact contiguous range requested. A short,
gapped, failed, truncated, or undecodable page is uncertainty and fails closed.
Paging bounds allocation without imposing the 10,000-event in-memory tail budget
on a valid long-lived stream.

Once a journal exists, it owns lifecycle even when a stale snapshot has a larger
numeric sequence. A valid snapshot may provide legacy baseline fields, but journal
events overlay it and a terminal tombstone remains terminal through the captured
high-water. Snapshot-only state is authoritative only while the journal is empty.
Snapshot state is normalized and validated before use: top-level entity type/id and
status must match the requested entity and transition table, and `fields.Id` and
`fields.Status` are canonicalized from that validated identity. `Deleted` remains a
valid runtime terminal marker even though it is not a user-declared IOA state.

A snapshot whose provenance is proven against the captured journal is an exact
replay baseline: recovery replays only the contiguous journal tail after its
sequence. Replaying its covered prefix would double-apply relative updates and
regenerate persisted nondeterministic effect values. A legacy snapshot without
journal provenance is instead a migration input; recovery strictly replays the
captured journal lifecycle so an older terminal record cannot be hidden by newer
stale snapshot bytes.

The first journal write from a snapshot-only stream atomically appends a versioned
state materialization record followed by the requested domain record. The control
record is recognized only when both its event type and its full schema/payload
shape match, including identity, state coordinates, empty recent events, and the
bounded idempotency map. A legal domain action named
`Temper.Internal.StateMaterialization.v1` with any other payload remains a normal
domain event.

The event store validates the exact snapshot fence, commits the materialization
and domain records, and retires the snapshot in the same transaction. A delayed
snapshot writer is suppressed after a valid materialization record is durable.
This establishes an unambiguous journal generation without losing the snapshot
baseline. Composite batches use the same handoff for every snapshot-only stream.

Optimistic-concurrency recovery rebuilds from a clean source. It then re-checks
the durable idempotency map before re-evaluating an action, so a stale replica that
recovers the winning action returns that committed result instead of appending it
again. Actor bootstrap requires both `sequence_nr == 0` and
`total_event_count == 0`; a materialization followed only by a composite audit
record can therefore restart without fabricating a `Created` domain event.

Ordinary hydration permits an empty-range journal read failure only when the
captured boundary proves that no journal record exists. Any non-empty boundary,
source change, page failure, or decode uncertainty fails closed. This deliberately
trades availability for durable-source correctness.

**Why this approach**: numeric “newest wins” cannot distinguish an equal-sequence
source replacement and can resurrect a stale live snapshot over a terminal journal.
The materialization record makes snapshot retirement replayable instead of relying
on an out-of-band assumption.

### 3. Key backfill is exact, source-fenced, and contract-revision fenced

The repair universe is the union of events, snapshots, catalog rows, field-index
rows, and existing key rows. For each entity, repair reconstructs one stable source
generation, derives the complete current key set, and conditionally replaces the
entity's rows only while all captured facts remain true:

- target key-set signature and monotonic contract revision;
- exact journal boundary and terminal classification;
- exact snapshot presence, sequence, and bytes;
- current stream liveness and reconstruction coordinates.

A changed snapshot, append, tombstone, recreate, contract change, or concurrent
claim rejects the row repair. The next pass starts from current state. A terminal
journal tombstone outranks any later stale live snapshot/catalog. Catalog-only
state is accepted solely when both journal and snapshot are absent; field-index-
only state cannot reconstruct a complete entity and fails the type closed.

Backfill establishes its target signature before replay. Every source mutation
that affects reconciliation advances the type revision and removes the watermark;
each row repair and final publication compares both signature and revision. The
persisted signature includes the new derivation-contract version, forcing every
older keyed type through one complete pass. A conflicting durable claim is not
skipped: it prevents watermark publication until the underlying data is repaired.

**Why this approach**: a SQL-only cleanup cannot replay domain state, and checking
only at final watermark publication cannot undo stale row mutations already made
during a crossed pass.

### 4. Query projections have a durable dirty-source protocol

`entity_catalog` and `entity_field_index` are derived state. PostgreSQL and Turso
therefore maintain `query_projection_dirty(tenant, entity_type, entity_id)`:

1. A journal or snapshot mutation marks the entity dirty in the same transaction.
2. A projection repair carries an exact `ProjectionSourceFence`: journal high-water
   plus snapshot presence, sequence, and exact bytes.
3. A source-fenced upsert, delete, or catalog-only acknowledgement clears the marker
   in the same transaction only when the fence still matches.
4. An unfenced projection write for a source-backed entity re-marks it dirty. This
   includes a delayed old asynchronous delivery after a successful repair.
5. Exact cleanup of an unstable attempted row compares the full catalog row
   (status, fields, state, sequence). If it removes the row it marks dirty, so a
   later repair restores it; a concurrent newer catalog/EAV row is preserved.

After an exact source match, the source generation outranks the prior catalog
sequence. This lets a snapshot-only sequence 5 repair replace a stale compatibility
catalog sequence 10. Sequence monotonicity still rejects unfenced stale deliveries.

Catalog-only ADR-0077 rows remain supported. When no journal or snapshot exists,
repair clears the seeded marker without deleting the row; ordinary unfenced writes
to such a row do not create an unrecoverable marker.

Before a native OData read trusts catalog or EAV data, it enumerates at most
`repair_budget + 1` dirty IDs, repairs one bounded batch, then checks the ledger
again. If work remains it returns a retryable `503 ProjectionUnstable`, but the
completed batch stays repaired. Repeated reads therefore make monotonic bounded
progress rather than failing forever before doing work.

The PostgreSQL migration creates the ledger, installs event/snapshot/catalog
mutation triggers, and seeds the union of existing events, snapshots, and catalog
rows in one migration transaction. The triggers bridge source writes and delayed
projectors from an older binary between migration and process drain. Turso checks
for the ledger, creates it, creates its index, and performs the same one-time union
seed in one `Immediate` transaction; a crash before commit leaves no partially
published table, so startup retries the whole migration.

**Why this approach**: a source fence prevents a stale repair, while the durable
marker prevents a source write or delayed projection from becoming a permanently
silent false negative after a crash.

### 5. Reads give co-committed ownership precedence

An exact declared-key filter consults the key index only after the current contract
signature is fully published. Coverage and lookup are fenced by the monotonic
contract revision. A revision change discards the proof and falls back to the
bounded authoritative scan.

Before coverage completes, neither an old hit nor an old miss is authoritative.
Recognized key queries bypass asynchronous projection authority and scan the stable
journal/snapshot sources within the configured budget. A large incomplete type may
return 413; an unstable source or dirty-projection repair may return 503. Neither is
converted into a false-empty success. Replayed `Deleted` state is absent from every
read mode and schedules idempotent projection removal.

Even under complete coverage, a key-row hit is closed against the recovered fields,
not only its numeric sequence. Same-sequence snapshot replacement can preserve the
row's sequence while changing its declared-key values; such a mismatch is unstable
ownership and cannot certify the stale owner.

**Why this approach**: key ownership and journal state co-commit; catalog/EAV data
converges later. The stronger source must decide identity and liveness.

### 6. Tenant runtime generations publish behind one admission barrier

A tenant's specs, CSDL, cross-invariants, declared-key activation epochs, Cedar
policies, reaction graph, WASM name-to-hash mappings, OS-app content, seed data,
and in-progress installed-app publication record form one runtime generation.
In the single-process runtime they share a per-tenant asynchronous read/write
barrier:

- admitted requests retain a read lease for the complete dispatch lifetime;
- a publication acquires the write side without unbounded waiting;
- arming the writer evicts resident actors and closes direct actor resolution;
- the writer persists first, activates every in-memory component, creates required
  app content and seed entities, reopens the tenant, and schedules post-cutover
  reconciliation before any cancellable acknowledgement wait;
- terminal `installed` metadata is a derived recovery marker written after that
  cutover, so a lost acknowledgement or cancellation cannot retain an
  unreconstructible sticky intent; and
- completion advances a monotonic in-memory generation number.

Arming also records a content-derived publication intent and sticky debt. Once a
durable mutation begins, cancellation, an error, or a lost acknowledgement cannot
prove that nothing committed. The tenant therefore remains gated after the writer
unwinds. Only a serialized retry of the exact same intent may discharge that debt;
an unrelated or partially matching publication cannot reopen traffic.

The publication workflow itself needs to spawn and dispatch actors after the new
registry is installed but before external traffic resumes. It receives a released
`TenantGenerationLease` bound to the writer's tenant and captured generation.
Actor resolution accepts that context only while the tenant is gated at exactly
that generation, and it never bypasses an unfinished declared-key activation.
Unscoped callers and contexts from retired generations remain fenced. Scheduled
actions, state timeouts, compensations, reactions, and protocol streams retain or
capture their originating generation; detached work is discarded rather than
executed against a replacement generation.

Direct WASM mutations authorize both before and after acquiring the writer, so a
policy cutover cannot be crossed between authorization and publication. WASM reads
hold the generation reader across authorization and registry/history access.

This barrier is intentionally process-local. The supported runtime topology for
this release is one server process per tenant store. A multi-process runtime must
replace it with a durable distributed generation lease before horizontal writers
are enabled.

**Why this approach**: individually atomic database rows do not prevent a request
from mixing old actors or authorization with new specs, reactions, or modules.
One admission boundary makes the complete runtime generation the unit of visibility.

### 7. Granular policy ownership and Genesis provenance join OS-app publication

Granular Cedar rows are the canonical durable policy source. An OS app owns a
stable row set identified by `created_by = "os-app:<name>"`; its publication
atomically replaces that owner's complete set while preserving other owners. The
legacy aggregate tenant-policy blob is regenerated in the same transaction as a
read cache. Startup reconstructs from granular rows when they exist, so a crash
cannot promote a partial row-by-row update or revive a removed app policy.

Governance-decision policy effects publish one stable decision-owned row through
the same durable policy generation helper. The post-action hook is told when the
API already performed that publication and must not create a second authority.
Persistence failures remain request failures rather than successful but
non-durable approvals.

Genesis resolves the complete pinned dependency closure before installation and
constructs one `InstalledAppRecord` per resolved app. Its source kind, registry,
pinned/current hashes, follow policy, and closure identifier are passed into the
same atomic OS-app publication transaction; they are not patched after cutover.
All closure members reconcile serially under one retained tenant publication
writer, and every supplied provenance record must be consumed by that closure.

**Why this approach**: policy text, policy ownership, and installation provenance
decide what can run and what can be restored after restart. Post-cutover repair of
either is an observable split generation, not harmless metadata enrichment.

### 8. Composite retries use a content-bound durable batch claim

Composite dispatch no longer proves idempotency by scanning an unbounded parent
journal. The deterministic first append carries a `PersistenceBatchIdempotency`
claim containing the parent persistence id, caller idempotency key, and a digest of
the complete parent audit plus composite sub-write intent. The store checks and
co-commits that claim with the batch:

- the same key and digest returns the committed result without reapplying staged
  projections;
- the same key with different content is rejected; and
- a failed or conflicting batch publishes neither events, ownership, nor a claim.

Sim, PostgreSQL, Turso, and the supported single-stream Redis batch path implement
the same contract. PostgreSQL migration 0015 and Turso's transactional schema add
the durable claim table; Redis uses a namespaced claim key in the append Lua
transaction. A partially generated pack object retains its existing repair
capability by using an intent-qualified claim until the complete payload is ready.

Within the current single-process tenant runtime, callbacks sharing the exact raw
claim namespace are serialized from claim inspection through append by a bounded
weak lock registry. This closes the pre-append race in which both callbacks could
observe no claim and one would surface an optimistic conflict instead of replaying
the other's committed result. The durable store remains the final authority, and
different claim namespaces remain concurrent. A multi-process runtime must replace
this local admission serializer with a durable lease or reservation protocol.

**Why this approach**: bounded replay tails and journal paging are recovery tools,
not an idempotency index. A small co-committed claim makes retry cost independent of
stream age without weakening content binding.

### 9. Materialization and reconciliation consume explicit budgets

The snapshot-to-journal materialization envelope is bounded twice: its recovered
idempotency map retains only the deterministic newest entry budget, and the exact
borrowed reset-state representation must serialize within a 16 MiB payload budget
before any large clone or append occurs. The final candidate is checked again after
canonical `Id` and `Status` insertion.

Key reconciliation enumerates the durable union through a stable entity-id cursor
in pages of at most 256 and yields between entities. Coverage is published only
after every page reaches an empty successor. Redis journal-boundary compatibility
reconstruction likewise scans a captured high-water in bounded pages, validates
contiguous sequences, and installs the first terminal sequence with a high-water
CAS. Later boundary reads are constant-time, and a terminal record continues to
outrank any legacy live suffix.

**Why this approach**: replacing an unbounded allocation with a silent fixed prefix
would certify incomplete state. Explicit page and byte budgets retain progress while
making truncation or oversized state a visible failure.

## Rollout Plan

This release introduces a durable control record that older readers cannot replay.
It is therefore a drain-and-cutover release, not an ordinary overlapping rolling
deployment.

1. **Install the PostgreSQL migrations first** — migration 0014 atomically installs
   the dirty ledger, mixed-version source/catalog triggers, and upgrade seed;
   migration 0015 adds monotonic key-contract activation state and durable composite
   batch-idempotency claims. Old writers may continue briefly; the 0014 triggers keep
   their source and delayed projector writes dirty.
2. **Drain and stop every old reader, writer, and projector** — verify no old binary
   can serve or mutate an entity before enabling the new actor code. The triggers
   cover writes during the drain, but they cannot teach an old reader to interpret
   a state materialization record.
3. **Start only the new binary** — Turso deployments stop the single old process,
   run the atomic local migration on open, then start serving.
4. **Repair before authority** — dirty projection reads drain bounded batches, and
   keyed types remain scan-safe until their new contract watermark publishes.
5. **Observe** — monitor projection-unstable responses, dirty-ledger depth, key
   repair failures, conflicts, and coverage publication before declaring the
   cutover complete.

## Readiness Gates

- RED/GREEN history preserves the original delete/null-key ownership regression.
- Actor and composite regressions cover snapshot materialization, exact retirement,
  same-name domain-event collision, stale-replica idempotency, composite-audit-only
  restart, and snapshot identity/status normalization.
- Seeded DST covers tombstone precedence, paged high-water replay, truncation,
  equal-sequence source replacement, source/liveness faults, and reclaim races.
- PostgreSQL and Turso tests cover exact snapshot-byte fences, lower authoritative
  snapshot over higher stale catalog, delayed unfenced projection delivery, full-row
  cleanup CAS, Live/Deleted/Live transitions, catalog-only acknowledgement, migration
  seeding, and mixed-version PostgreSQL triggers.
- A bounded dirty set larger than the read budget proves first-request progress and
  next-request completion.
- More than one 256-entity key-repair page must reconcile before coverage publishes;
  a pre-metadata Redis stream must recover a terminal boundary beyond its first
  1,024-event page and retain the earlier tombstone over a legacy suffix.
- Publication regressions prove that unscoped actor resolution is closed while the
  writer is armed, only the exact current internal context may bootstrap content,
  retired timers cannot dispatch, and WASM reads/writes cannot cross generations.
- Composite retries prove exact replay and content mismatch behavior in Sim, Turso,
  PostgreSQL, and supported Redis topology without scanning the parent stream.
- Live local E2E demonstrates delete/null release and reclaim through the server API.
- Formatting, strict Clippy, readability, workspace tests, DST review, independent
  exact-GitHub-head review, Greptile, and CI all pass.

## Consequences

### Positive

- Declared-key ownership exactly follows current non-deleted state, including the
  authoritative empty set.
- Snapshot-only state survives its first journal write without an ambiguous source
  generation or stale-snapshot resurrection.
- Query projections cannot remain silently stale after a source write, repair race,
  migration, or delayed delivery.
- Long-lived streams recover through bounded pages without an artificial lifetime
  event limit.
- Key, actor, backfill, and query reads share the same source-authority rules.

### Negative

- Keyed writes replace the entity's reverse-index rows and take the type/stream
  locks required for atomic validation.
- Projection reads may return retryable 503 responses while bounded dirty work drains.
- PostgreSQL adds row-level source/catalog triggers during the compatibility cutover.
- The first deployment performs a full versioned key repair and a one-time projection
  seed.
- Deployment requires an old-binary drain because the journal format gains a durable
  materialization control record.
- Tenant publication is single-process until a durable distributed generation lease
  replaces the in-memory barrier.
- Exact composite-claim convergence is single-process until a durable distributed
  claim lease or reservation replaces its bounded in-memory serializer.

### Risks

- Publishing ownership before validating every claim could release a valid owner on
  a rejected batch. Stores validate the complete transaction before mutation.
- Clearing a dirty marker outside the exact source transaction could certify a stale
  row. The fence and clear are one transaction, and unfenced writes re-mark it.
- A partially introduced Turso ledger could permanently skip upgrade seeding. Its
  table, index, and seed share one immediate transaction.
- Overlapping an old reader with a materialized stream could lose the retired
  baseline during replay. Rollout requires a verified old-reader drain; database
  triggers mitigate old writes but do not relax that barrier.
- Treating a control event name alone as internal could drop a legal domain action.
  Full payload discrimination is required everywhere, including snapshot retirement.

### DST Compliance

- Simulation-visible code uses `sim_now()`/`sim_uuid()` and ordered collections.
- Replay and repair are bounded by explicit budgets and deterministic high-water
  coordinates; no thread or ambient I/O is introduced.
- The existing production-only retry sleep and timing metrics retain their scoped
  `determinism-ok` annotations.

## Non-Goals

- Making Turso declared-key ownership authoritative.
- Changing key hashing or general non-key OData semantics.
- Supporting an old reader after the first materialization record is enabled.
- Merging or deploying this arena PR before adjudication.

## Alternatives Considered

1. **Sentinel rows for empty ownership** — rejected because they would participate
   in uniqueness and encode invented domain values.
2. **Asynchronous key cleanup** — rejected because reclaim can cross the cleanup and
   journal/key ownership can diverge on failure.
3. **Numeric newest-source wins** — rejected because equal-sequence replacement and
   stale higher snapshots/catalog rows are valid historical shapes.
4. **SQL-only repair** — rejected because current state and tombstone precedence
   require transition-table replay.
5. **In-memory projection invalidation** — rejected because crash and multi-process
   delivery gaps require a durable transactionally maintained marker.

## Rollback Policy

Before any valid `Temper.Internal.StateMaterialization.v1` record is committed, the
new binary may be drained and the code rolled back; the key index and query projection
are derived data and can be rebuilt.

After the first materialization record commits, an ordinary rollback to an older
reader is unsafe: the authoritative snapshot may already be retired and the older
binary cannot reconstruct the baseline from the control record. From that point,
recovery is forward-fix only with a reader that understands this record. The only
rollback to an old binary is restoration of a full pre-enable database backup while
all processes are stopped, accepting loss of writes after that backup. No mixed old/new
reader overlap is permitted during either direction of the cutover.
