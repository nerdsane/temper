# ADR-0161: Uniform journal lifecycle, projection, and commit parity

- Status: Accepted
- Date: 2026-07-07
- Updated: 2026-07-11
- Deciders: Temper core maintainers
- Related:
  - ADR-0040: Segmented event history and bounded replay
  - ADR-0154: OData read-surface truthfulness
  - Linear ARN-192

## Context

The `EventStore` implementations disagreed about what their entity-list methods
returned. Turso and Postgres filtered some tombstones in some queries, while Redis,
Sim, and the whole-tenant queries returned every discovered stream. The SQL filters
also treated any historical `Deleted` event as final, so they hid a stream that was
legitimately recreated after deletion, and they did not recognize the runtime's
`payload.action == "Deleted"` tombstone form.

The first ARN-192 implementation moved tombstone classification into
`ServerState`, but did so by calling `read_events(pid, 0)` for every candidate. A
tenant with `N` entities and `M` lifetime events therefore performed `N` store calls
and decoded `N * M` events during boot or the first collection read. Worse, a read
error was interpreted as "not deleted," so uncertain entities were indexed as live
and the type was marked hydrated.

Redis had a separate atomicity defect. `append` committed the journal list and
sequence in Lua, then updated segment metadata through fallible Redis calls. A
post-commit error was reported as an append failure even though the events were
durable, leaving the actor on a stale sequence and making a retry duplicate work.
The initial repair stopped updating segment metadata on append, but `save_snapshot`
continued to read and rotate that metadata. This left a second, divergent history
index that was both incomplete and unused by recovery.

Turso had the inverse problem: its multi-event append updated journal and segment
rows in one transaction, its single-event path committed the journal before a
fallible segment update, and its multi-stream batch omitted segment maintenance.
One backend therefore exposed three durability contracts for the same operation.

## Decision

### Raw discovery has one meaning

`list_entity_ids`, `list_entity_ids_by_type`, and `list_entity_ids_limited` discover
raw event-journal candidates. They do not decide whether a stream is live.
Whole-tenant, typed, and limited variants use the event journal as their only
discovery source and include tombstoned streams consistently. Projection catalogs
and field indexes are derived data and never create an entity-discovery candidate.

Callers that must preserve history, such as Turso-to-Postgres migration, continue to
use raw discovery and therefore include deleted journals.

### Tail classification is a required batched store primitive

Every backend implements `read_latest_events(persistence_ids)`. The contract is:

- input order is preserved and exactly one `Option<PersistenceEnvelope>` is returned
  per requested persistence id;
- only the latest event is read and decoded;
- requests are bounded to `LATEST_EVENT_BATCH_SIZE` entries;
- corruption, transport failure, or a contract-length mismatch is an error, never a
  live result;
- `None` means the requested persistence id has no journal. It is valid for a direct
  probe, but it is a consistency error when classifying a candidate returned by a
  discovery method.

Postgres and Turso issue one indexed query per batch, Redis pipelines `LINDEX -1`,
and Sim reads the final in-memory envelope directly. No backend reads a full journal
to classify liveness.

The canonical predicate `is_deletion_tombstone` lives beside the persistence types.
It recognizes `event_type == "Deleted"`, `payload.action == "Deleted"`, and the
composite-event form `payload.to_status == "Deleted"`. A raw tail may be a
`CompositeEvent` audit record that does not change the entity lifecycle. In that
case the shared classifier performs a bounded 1,024-event lookback for the latest
non-audit lifecycle event while retaining the raw tail sequence as the freshness
proof. The store receives a hard limit of 1,025 rows, so a writer extending the
suffix after the tail probe cannot force an unbounded decode; the extra row detects
that race and fails closed. Every returned sequence must be contiguous. A missing
lifecycle event, internal gap, or incomplete lookback is an error.

The shared server enumeration helper chunks candidates, reads their lifecycle
tails, applies the predicate, and returns an error unless the entire classification
succeeds. Only a successful classification may mutate the entity index. This is
fail-closed: uncertainty cannot resurrect an entity or bless a partial index as
authoritative.

### Index scans publish under scoped epochs

Store enumeration and tail classification perform I/O, so a concurrent create or
delete can otherwise be overwritten by a stale scan result. Every synchronous
index mutation advances both a tenant epoch and the affected tenant/type epoch. A
whole-tenant scan captures the tenant epoch; a typed scan captures the type epoch.
Publication holds the epoch mutex, verifies the captured value, updates the index
and hydration watermark together, and advances every scope it changed. A mismatch
rejects the publication and requires retry. Unrelated tenants do not invalidate a
typed scan.

Eager actor hydration publishes classified candidates with the affected hydration
watermarks cleared. It restores those watermarks only after every actor has spawned
and answered successfully. Any mid-loop failure therefore leaves collection reads
retriable instead of making a partial index authoritative. Actor-backed create
publishes its index entry only after the actor proves the bootstrap append is
durable, then advances the same epoch.

Epochs coordinate scans within one process; they cannot observe a writer in another
server process. Consequently, correctness-sensitive lazy lists reconcile with raw
durable discovery on every call when a journal is configured, even after a local
hydration watermark exists. A list materializes at most 100,000 raw candidates and
requests one extra row to detect overflow. Exceeding that budget is an explicit
error, never a partial list.

### Projection backfill always replays the journal tail

The query-projection backfill never publishes a deserialized snapshot directly.
For every journal-discovered candidate it loads the latest snapshot and strictly
replays all later events. Snapshot reads, journal reads, and schema-incompatible
entity payloads are errors in this strict path. A deletion after the snapshot
removes the projection, so a stale live snapshot cannot resurrect a tombstoned
entity. Any recovery error quarantines an existing projection rather than leaving
derived state visible as if it had been verified.

Strict recovery verifies that every returned sequence is contiguous and compares
the recovered final sequence with a separate raw-tail read. A store that
successfully returns a truncated prefix therefore cannot publish partial state.
`CompositeEvent` payloads are decoded strictly in this path.

Replay is generation-aware. A tombstone ends the current generation. Audit-only
composite records may follow it without resurrecting the generation; the only
lifecycle event permitted after it is a complete `Created` envelope whose decoded
action is `Created`, whose source status is empty, whose target is the current
spec's initial state, and whose parameters are an object. It resets fields,
counters, lists, booleans, and idempotency state before replay continues. Any other
lifecycle event after deletion is journal corruption and recovery fails.

### Derived read candidates require journal sequence parity

Field-index, catalog, and declared-key rows are derived candidates, not authority.
Before OData materializes or authorizes them, the server batch-reads their journal
tails. Missing streams and tombstones are omitted; read failures return a sanitized
`503 StorageUnavailable`. Backend details are logged server-side and are never
included in the client response.

For a live candidate the catalog row is usable only when its `sequence_nr` equals
the validated journal tail. A mismatch falls back to actor recovery. The validated
minimum sequence is retained through that fallback: if an already-running actor
lags an out-of-band or remote append, a strict refresh is serialized through that
actor's mailbox. This avoids overlapping actor generations while ensuring later
commands use the proven durable state. The read fails rather than authorizing or
re-projecting state below the proven sequence. Each candidate batch is tail-read
once; native-page budgeting counts all projection rows, including stale/orphan
rows, without a second per-entity liveness probe.

Single-entity reads use the same contract, including deployments with no query
plane. When a journal exists, stale or absent materialized state falls back to
strict snapshot-plus-tail recovery rather than lenient actor replay. A stale
in-memory index without a journal returns 404 and cannot bootstrap a `Created`
event. Collection enumeration errors propagate instead of becoming empty lists.
Consumers with executable caches, such as `HttpEndpoint`, clear the affected cache
on uncertainty rather than keep serving stale routes.

The same journal-tail gate protects security-sensitive decisions. Existing-entity
Cedar snapshots, context-entity status, account verification, app-name uniqueness,
storage-cap accounting, relation checks, file-container checks, reactions, and
cross-entity guards never trust a process-local status or existence cache when a
journal is configured. A durable absence remains an ordinary absent optional
reference where the spec permits that meaning; a read or replay error is
uncertainty and fails closed. Reaction authorization treats a durable absent target
as a synthetic initial resource only for a true `Create` action, using the generated
target id and effective create parameters. If that id is already live, authorization
uses its authoritative existing fields instead, so create-shaped input cannot
replace owner or scope attributes. Non-create absence remains an error. This keeps
FileVersion-style create cascades functional without weakening the existing-entity
tail gate. In-memory-only deployments retain their spec-defined bound-action
materialization behavior because there is no remote journal that can diverge.

Composite sub-writes use the same rule. They load the authoritative target before
authorization, authorize a live collision against that exact preflight state, and
reuse it for transition validation and the later sequence compare-and-set. Only a
proven-absent `Create` uses synthetic initial attributes. Reaction, composite,
collection, content-addressed, and atomic File creates share one server-level
attribute builder. It removes all runtime-owned aliases, then publishes proven
`id`/`Id`, `status`/`Status`, `has_spec`, and declared context-entity statuses.
Collection ingress rejects conflicting, non-string, or empty `id`/`Id` aliases and
a supplied lifecycle value that differs from the spec's initial state.

Collection create is a compare-and-append operation, not a lookup followed by
`get_or_create`. A live durable stream produces a typed `AlreadyExists` outcome;
the handler rebuilds the winner's authoritative Cedar resource, re-authorizes that
real identity and ownership, and returns `409` only when the caller may observe the
collision. It never returns the existing entity as a synthetic `201`. Concurrent
creators compare at sequence zero (or at the proven raw tombstone tail for a new
generation), so exactly one `Created` event wins. PostgreSQL's native data-only
path returns the same typed outcome while keeping its first event and projection
in one transaction. PostgreSQL actor-runtime inserts use `INSERT ... RETURNING`
to distinguish an inserted actor from `ON CONFLICT DO NOTHING`; a collision cannot
overwrite fields or masquerade as the caller's create.

The runtime-owned field contract is shared by the spec parser and server. Specs
cannot declare mutable state or action parameters named `id`, `Id`, `status`,
`Status`, `has_spec`, `HasSpec`, or `ctx_*_status`. Action processing, actor
bootstrap, data-only creation, composite writes, OData field updates, replay, and
snapshot restore all enforce the same contract. Durable entity fields publish both
supported schema casings (`id`/`Id` and `status`/`Status`), but all four are
regenerated from the actor's authoritative identity and lifecycle state on every
mutation. Legacy event or snapshot payloads are canonicalized during recovery
instead of preserving caller-controlled aliases as a second truth.

Declared context entities are security inputs, so the production hand-written IOA
parser preserves and validates every `[[context_entity]]` block. It no longer
silently discards those blocks while the serde parser accepts them. Cedar context
statuses are resolved from the referenced entity's authoritative durable status;
caller-supplied `ctx_*_status` values are never trusted.

Authorization resource construction has one implementation for reads and writes.
OData entity reads and PostgreSQL actor-backed mutations no longer flatten a
response through a second synchronous adapter that silently maps registry errors
to `has_spec = false` or omits declared context status. They call the same async
server builder with a proven identity, lifecycle, and field object; uncertainty in
registry or context state is a sanitized availability error rather than a changed
Cedar resource.

### Generic field updates are durable events

OData PATCH and PUT previously mutated only actor memory and the derived query
projection. A restart replayed the older journal and silently reverted the write.
They now append `FieldsPatched` and `FieldsReplaced` events through the same
optimistic sequence and declared-key co-commit path as spec actions. Replay applies
those event types through the shared field synchronizer; PUT clears ordinary fields
before applying its replacement, while both forms preserve canonical identity and
lifecycle fields. Request bodies must be objects, identity aliases must match the
path, lifecycle aliases must match current state, and runtime-owned fields are
removed before invariant checks and persistence. A failed append rolls actor state
back and cannot publish a projection-only success.

An allowed update is bound to the exact state Cedar evaluated. The target actor
compares a deterministic digest of its sequence, lifecycle, and recursively
canonicalized fields inside its mailbox. Every distinct durable context stream
that supplied a `ctx_*_status` also contributes a `PersistenceSequenceGuard`.
`append_batch_guarded` compares those sequences at the serialization point of the
same commit that appends the target event and replaces declared keys. PostgreSQL
uses a serializable transaction, Turso an immediate transaction, Redis one Lua
script, and Sim one deterministic store lock. A context creation is guarded by
expected sequence zero, so absence-dependent decisions are protected too. A
target or context mismatch is a `409 ConcurrentModification`; no field event is
written. A context-dependent field mutation in an in-memory-only deployment has
no cross-actor commit point and therefore fails closed instead of pretending that
a second read is atomic.

Field projections follow the existing bounded, coalescing projection queue after
the journal commit. The worker retries transient failures and records retry/error,
queue-full, and terminal-exhaustion telemetry. The HTTP write acknowledges the
authoritative journal commit; it never reports that source write as failed and
invite a client retry that would append a duplicate `FieldsPatched` or
`FieldsReplaced` fact. Journal-gated collection reads and parity backfill remain
the repair path if the bounded queue cannot retain or complete the derived update.
Each field mutation also carries one stable durable idempotency key across the
bounded actor-ask retry loop, so a lost actor reply cannot append the same source
event twice before the projection worker even runs.

Projection deletion is sequence-guarded. A delete at sequence `D` may remove only a
catalog row whose projected sequence is at most `D`; it cannot erase a recreated
generation already projected above `D`. The direct, queued, composite, retry, and
quarantine paths all use the same guarded operation. The query-plane trait requires
this operation explicitly; adapters cannot inherit an unguarded fallback.

### Delete cleanup is atomic where required and retryable where derived

Postgres and Sim replace an entity's complete declared-key claim set in the same
commit as its journal append, for both single-stream appends and atomic
multi-journal appends. This removes cleared or renamed keys, permits atomic key
swaps, and removes all claims on either direct or composite deletion. Governed
batch members carry `Some(desired_key_rows)`; raw or migration batches carry
`None`. Backfill conflicts are errors and do not advance a watermark past an
unresolved duplicate.

The key replacement mode is explicit. `Some(rows)` is a complete replacement,
including `Some([])` when a governed transition intentionally clears every key;
`None` is a raw append that has no post-transition fields and therefore preserves
existing claims only while it stays within one entity generation. Any raw batch
that crosses a tombstone retires the prior generation's claims, even if a later
event recreates the stream, because the raw caller cannot prove the new keys.
Trailing `CompositeEvent` audit records do not hide a terminal tombstone;
authoritative rows are inserted only when the final non-audit lifecycle event is
live. A raw audit or externally replicated append can no longer silently turn an
authoritative keyed lookup into a false absence or carry stale keys across a
generation boundary.

Query projections are asynchronous derived state and cannot be transactionally
co-committed with every journal backend. If projection removal fails after a
tombstone commits, the delete returns a sanitized retryable error after evicting the
actor and in-memory index. A retry classifies the durable lifecycle first and runs
only sequence-guarded projection and legacy-key cleanup. It deliberately skips the
original transition authorization, relationship, and state preconditions, which
may no longer be satisfiable after deletion, and does not append a second tombstone.

Delayed declared-key cleanup is generation-aware. PostgreSQL removes a legacy key
row only through the tombstone sequence and only when no later `Created` event
started a replacement generation. Delayed cleanup does not evict an actor or index
entry that a recreated generation may now own.

Restrict-delete relationship checks strictly recover every discovered source
entity. A listing or replay failure returns a sanitized retryable error rather than
being skipped or reported as a domain conflict. A check scans at most 512 live
source candidates per relationship; overflow fails closed.

### Append results are reconciled at the production store boundary

An empty append or empty batch member is a no-op on every backend: it returns the
expected sequence and creates no journal, segment, discovery, or key state.

A distributed store can commit and then lose the success acknowledgement. For raw
journal-only appends, the server's `BoxedEventStore` wrapper reconciles an append
error with a storage-bounded read of at most the attempted event count plus one,
followed by a direct latest-tail probe. It compares sequence numbers, event type,
payload, and every metadata field, and reports success only when the durable suffix
contains exactly the attempted events and the independent tail proof ends at the
same envelope. This remains fail-closed even if a backend returns a silently
truncated range. Batch tail probes are chunked to the shared latest-event budget;
every non-empty stream must match. An all-empty failed batch has no durable commit
to prove and remains an error.

Journal equality does not prove a declared-key replacement: the same event envelope
can accompany K1, K2, an explicit `Some([])` clear, or a `None` preserve intent.
Consequently, `append_with_keys` errors and any failed batch containing
`key_rows: Some(_)` are never converted to success from journal evidence alone.
They preserve the backend error until an indexing store exposes an authoritative
complete-key-set proof. Raw `None` batches remain reconcilable because their key
semantics are precisely to preserve whatever claims are already durable.

The same rule applies to guarded appends. A durable target suffix cannot prove
that every independent context guard passed in the same transaction, so the
production wrapper never reconciles a guarded backend error from journal bytes.
Guard and append ids are canonicalized and must be distinct; compare-only batches
without at least one durable target event are rejected.

The wrapper validates unique logical persistence streams before invoking a backend,
and the reconciler repeats that structural check before examining any durable
events. Validation canonicalizes the legacy `type:id` form to the same default-
tenant stream as `default:type:id`. A duplicate empty member, alias, or duplicate
already-durable event therefore cannot be converted from a rejected batch into
success. Otherwise reconciliation preserves the original error. Raw backend
consumers outside this production wrapper still receive the backend's ambiguous
error and must apply their own retry policy.

Turso no longer has a separate single-event durability shortcut. Single-event,
multi-event, and multi-stream appends write journal rows, segment assignment, and
segment counters in one immediate transaction. A segment failure rolls back every
journal row in that append or batch. Single-event appends use the same bounded
process write lane as other transactions; bypassing it allowed overlapping libSQL
transactions to fail with API-misuse errors under parallel local E2E load.

Postgres, Turso, and Sim maintain the same segment boundary rules for both single
and batch appends. There may be only one open segment and it must be the highest
known segment; a missing, duplicate, or out-of-order open segment is an error rather
than a reason to mutate a sealed segment. Snapshot-only streams do not invent event
segments. A snapshot rotates a segment only when its sequence is the current
journal tail, and the latest recovery snapshot advances monotonically even when an
older asynchronous writer finishes later. Historical snapshots are still retained.

Sequence arithmetic and signed database conversions are checked before mutation.
Postgres and Turso reject values outside their signed 64-bit storage range, and
negative persisted journal, snapshot, or projection sequences are corruption rather
than being coerced to large or zero values. Redis additionally caps sequences at
9,007,199,254,740,991, the largest integer Lua 5.1 can compare exactly, and validates
both client inputs and Lua-side stored sequence values.

### Composite idempotency has an explicit durable window

Composite dispatch checks the raw tail and reads only the most recent 1,000 events
when recovering a parent idempotency key. The backing store is given a 1,001-row
limit, so a concurrent tail extension is detected without allocating an unbounded
suffix. It verifies the exact expected row count, every contiguous sequence, and the
proven tail; a gap, truncation, or later event fails closed. This matches the actor's
bounded durable idempotency-key budget and prevents lifetime-journal scans.

### Sim uses the same physical stream identity as durable stores

Sim canonicalizes every persistence-id map key to `tenant:type:id`. The legacy
`type:id` form and `default:type:id` therefore share one journal, CAS sequence,
snapshot/history, segment record, injected-fault queue, and append-delay queue even
when aliases are used in separate calls. This matches PostgreSQL, Turso, and Redis;
the deterministic reference store cannot hide alias races that production rejects.

### Redis keeps one authoritative history representation

The Redis event list, sequence key, latest snapshot, and immutable snapshot-history
record are sufficient for recovery and audit through the `EventStore` contract.
Redis segment records have no reader, are not exposed by the trait, and duplicate
facts already present in the journal and snapshot boundaries. They are removed from
both append and snapshot paths. Existing segment keys are harmless orphaned data and
are ignored.

The append Lua script remains the single commit point for journal entries, sequence,
and entity discovery. It dual-writes the legacy discovery set and lexicographically
ordered global and per-type sorted sets. The limited-list API uses `ZRANGE`, so its
`limit` bounds both returned rows and steady-state storage work. Existing discovery
sets migrate through bounded `SSCAN` pages followed by idempotent `ZADD` batches;
new appends dual-write during migration. Limited reads compare legacy and ordered
cardinality even after migration, so a legacy-only writer during a rolling upgrade
causes the next read to repair the ordered indexes. After Lua returns success,
`append` performs no fallible work.

Snapshot recovery is proven through the observable contract: save/load the latest
snapshot, read the journal tail after its sequence, and continue appending at the
committed sequence.

Redis saves the monotonic latest snapshot and its sequence-specific history record
in one Lua operation. A failure cannot acknowledge only one half, and a delayed
older snapshot remains history without regressing the recovery boundary.

## Consequences

### Positive

- Boot and first-list classification normally decodes one event per entity; an
  audit-only tail uses a bounded lifecycle lookback rather than lifetime replay.
- All backends expose the same raw discovery semantics and use the same tombstone
  predicate, including delete-then-recreate streams.
- A failed or corrupt tail read never indexes an uncertain entity and never marks a
  partial type hydrated.
- Concurrent index scans cannot overwrite an in-scope create or delete, and eager
  hydration failures cannot publish partial completeness. Durable reconciliation
  also observes creates and deletes performed by another server process.
- Projection backfill cannot publish a stale snapshot without its journal tail.
- Corrupt payloads, contradictory tombstones, and invalid post-delete generations
  cannot publish or retain a projection.
- OData never authorizes or filters using a derived row older than the validated
  journal tail, and storage diagnostics are not exposed to clients.
- Durable reaction creates authorize a proven-absent target from synthetic initial
  attributes, while collisions authorize against the live target; non-create
  reactions cannot turn absence into an initial-state resource.
- Composite creates use the same live-versus-absent proof, and neither request nor
  persisted fields can shadow trusted Cedar identity, lifecycle, context, or spec
  attributes.
- Every create ingress uses one authoritative Cedar builder, and the IOA production
  parser cannot silently omit declared context entities.
- PATCH and PUT survive actor/server restart because their canonical field changes
  are journal facts rather than projection-only mutations.
- Tombstones release declared keys atomically; projection-delete failures remain
  safely retryable without duplicate journal events.
- Sequence-guarded projection removal cannot erase a newer recreated generation.
- Delayed key cleanup cannot retire a recreated generation's declared keys.
- Lost commit acknowledgements for journal-only intents are accepted only after
  bounded, exact durable-tail reconciliation; key-bearing intents, later events,
  and structurally duplicate batches remain errors without a complete proof.
- Turso journal rows and segment metadata cannot commit independently for any
  append shape.
- Postgres, Turso, and Sim cannot regress snapshots or rotate segment metadata at
  a stale or nonexistent journal boundary; Sim batch appends update segments too.
- Sim legacy/default persistence-id aliases share one physical stream across calls,
  including CAS, faults, snapshots, and segment state.
- Sequence overflow, signed conversion, and Redis Lua rounding cannot silently
  reorder a journal or bypass a sequence-guarded projection cleanup.
- Redis cannot report failure after committing an append, and no incomplete segment
  index remains to disagree with the journal.
- Redis latest-snapshot and history writes are atomic and monotonic.
- Redis limited discovery is ordered and storage-bounded after its one-time,
  bounded-memory legacy-index migration.
- The new primitive is directly contract-testable without constructing a server.

### Tradeoffs

- Raw store listing includes tombstones. Callers asking for live entities must use
  the shared classification path rather than assuming a list method is live.
- Correctness-sensitive lists deliberately repeat bounded durable discovery instead
  of trusting a process-local hydration marker in a multi-writer deployment.
- The 100,000-candidate discovery budget, 1,024-event audit-tail lookback, and
  512-candidate restrict-delete budget turn oversized operations into retryable
  failures. Operators must partition or repair data rather than receive a partial
  answer.
- Query-plane-only rows are not entities for discovery purposes. Operators must
  repair or remove orphaned projection data instead of relying on it to synthesize a
  missing journal.
- OData catalog/native reads pay one bounded latest-tail lookup per candidate page.
  Stale catalog rows additionally require actor recovery, but the validated tail is
  reused so there is no extra per-entity liveness probe.
- Strict backfill intentionally quarantines an entity when its persisted payload no
  longer decodes. Strict request-time and backfill reads also pay a tail lookup to
  prove sequence completeness.
- Composite idempotency is bounded to the most recent 1,000 parent events. A key
  older than that window is no longer a durable duplicate proof; callers requiring
  longer deduplication must provide a higher-level durable operation identity.
- Ambiguous-commit reconciliation is a server-wrapper guarantee. Direct users of a
  raw backend do not inherit it automatically.
- A lost acknowledgement for a governed key replacement may leave a write durable
  while the caller receives an error. Retrying is safer than falsely acknowledging
  the wrong key set; eliminating this availability tradeoff requires a new
  authoritative complete-key-set read primitive.
- Turso single-event appends now pay the same explicit transaction round trips as
  multi-event appends in exchange for one atomic journal/segment truth.
- Redis rejects a stream before sequence 9,007,199,254,740,992. That boundary is
  far above the per-entity event budget and avoids unsafe Lua numeric rounding.
- Redis retains both the legacy discovery set and ordered sorted sets so rolling
  upgrades and full unbounded list APIs continue to work. This duplicates only the
  small stream reference, not journal data.
- The first limited Redis read for a legacy tenant performs an `O(N)` incremental
  migration. Each client page is bounded, and steady-state reads are `O(limit)` plus
  constant-time cardinality checks.
- Existing Redis segment keys are not migrated or deleted automatically. They were
  never observable through `EventStore`; an operator may remove them offline.

## Validation

- Shared contract tests cover event-type, payload-action, and composite-status
  tombstones; audit-only tails; resurrection; fail-closed missing journals;
  deterministic input ordering; empty single and batch appends; and the batch
  budget.
- Sim and Turso tests always run locally. PostgreSQL and Redis integration suites run
  when their documented services are available; absence of a service is reported,
  not counted as passing evidence.
- Redis tests cover append conflict/commit behavior, bounded ordered discovery,
  legacy-set migration, empty-append behavior, and snapshot-plus-tail recovery
  without segment keys. Unit coverage also fixes the exact Lua integer boundary.
- Turso fault tests install a failing segment trigger and prove both single-stream
  and multi-stream journal inserts roll back, then succeed cleanly on retry.
- Sim, Turso, and live PostgreSQL tests cover batch segment maintenance,
  snapshot-only streams, delayed snapshot writers, monotonic recovery boundaries,
  and exact sealed/open segment ranges. Live Redis tests cover atomic monotonic
  latest/history snapshot writes.
- Server tests inject a latest-event failure and assert that no candidate is indexed
  and the type remains unhydrated. They also cover scoped publication races,
  delayed create publication, cross-process create/delete visibility, mid-loop
  eager-hydration failure, and a deletion tail after a live snapshot.
- OData tests cover orphan/tombstoned projections, stale-only candidate budgets,
  pagination across mixed stale/live rows, serialized stale-actor strict refresh,
  keyed deletion, sanitized 503 responses, and missing-journal direct GET.
- Security regression tests prove a remote authorization-field change blocks a
  mutation and a remote owner suspension blocks a guarded repository create even
  while the original actor generation remains running. TemperFS lineage E2E proves
  an absent FileVersion create target is authorized and cascades back to the File.
  Additional regressions prove reaction and composite create parameters cannot
  spoof trusted resource attributes and a composite create collision authorizes
  against the live durable target. Collection and content-addressed create tests
  cover the same shared builder, conflicting identity aliases, non-initial
  lifecycle input, authoritative context status, and reserved-field stripping.
- OData PATCH/PUT regression coverage rejects identity and lifecycle mutation,
  inspects canonical journal payloads, restarts against the same store, and proves
  both merge and replacement semantics replay exactly.
- Replay/backfill tests cover both tombstone encodings, delete/recreate field reset,
  invalid events after deletion, malformed recreation, successful truncated reads,
  sequence gaps, audit records after deletion, schema-corrupt payload quarantine,
  guarded deletion versus recreation, and retry of a failed projection removal
  without a second tombstone.
- Store/server tests cover complete key replacement, atomic key swaps, composite
  key lifecycle, generation-guarded delayed key retirement, duplicate-key backfill
  failure, raw/batch key preservation versus explicit empty replacement,
  tombstones with audit tails and raw generation-boundary retirement, lost
  single/batch commit acknowledgements, truncated-range/later-tail and
  duplicate-batch rejection, K1-versus-K2 and clear-versus-preserve ambiguous key
  intents, mixed raw/key-bearing batches, bounded composite idempotency, relation-read failure,
  relation-scan overflow, storage-bounded audit lookback under a moving tail, and
  cross-call Sim alias parity for CAS, reads, faults, snapshots, and segments.

## Alternatives Considered

1. **Filter tombstones independently in each backend.** Rejected because it
   duplicates a domain predicate in SQL/Lua/Rust and already diverged on payload
   tombstones and resurrection.
2. **Read each complete journal in the server.** Rejected because it is an
   `N * M` cold-path amplification and makes a transient read failure easy to
   mishandle.
3. **Keep Redis segment records and update them in the append Lua script.** Viable,
   but rejected because the records have no reader or trait-level behavior. Making
   an unobservable duplicate index atomic adds code without adding capability.
