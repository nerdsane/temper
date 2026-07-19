# ADR-0170: Journal OData field updates as entity events

- Status: Proposed
- Date: 2026-07-13
- Deciders: Temper core maintainers
- Related:
  - ADR-0046: Optimistic concurrency recovery for entity writes
  - ADR-0153: Declared composite key index
  - ADR-0155: Declared vector access path
  - `crates/temper-server/src/entity_actor/actor.rs`
  - `crates/temper-server/src/state/entity_ops.rs`
  - `crates/temper-server/src/odata/write.rs`
  - `crates/temper-runtime/src/persistence/mod.rs`
  - `crates/temper-store-postgres/src/store.rs`
  - `crates/temper-store-sim/src/lib.rs`

## Context

OData PATCH and PUT reach `EntityMsg::UpdateFields`, which currently mutates an
actor's in-memory `EntityState::fields` and immediately reports success. Unlike
state-machine actions and deletion, that write appends no journal event and does
not advance the snapshot boundary. Actor eviction or process restart therefore
rehydrates the entity without the acknowledged field update. A PATCH-only entity
loses every edit because no later action happens to create durable history.

Persisting only a snapshot would close the immediate restart gap, but it would
make field writes depend on a second persistence mechanism and omit them from the
ordered entity history. Field updates instead need the same durable ordering as
the entity's other writes.

## Decision

### Sub-Decision 1: PATCH and PUT append a reserved field-update event

Each successful `UpdateFields` message appends one entity-journal envelope with a
reserved internal event type and a versioned payload containing:

- the caller's field object; and
- whether the operation is merge (PATCH) or replacement (PUT).

The event retains the entity's current status as both its source and target
status. It is an entity mutation, but not a state-machine transition, so replay
must recognize the reserved event type before transition-table lookup. Keeping
the operation explicit preserves PUT replacement semantics; storing only the
resulting object would erase the caller-visible history of how the state changed.

**Why this approach**: one ordered journal remains the source of truth for
actions, deletion, and direct OData field writes. The actor rejects dispatch of
an automaton action whose name equals the reserved event type, so ordinary action
history cannot enter the field-update replay decoder.

### Sub-Decision 2: build a candidate state and commit it atomically

The actor clones its current state, applies the PATCH or PUT to that candidate,
and passes the candidate to the existing journal append path. This has three
properties:

1. journal append and declared key/vector rows are derived from the updated
   fields, so projections match the committed entity state;
2. the live actor state is replaced only after the append succeeds; and
3. serialization, storage, and optimistic-concurrency failures leave the
   caller's speculative PATCH/PUT fields unpublished and return a failed
   response.

Every append error first recovers the latest durable entity state. That
authoritative history becomes the actor's live state even when a later retry
fails or exhausts its budget; only the uncommitted PATCH/PUT fields are
discarded. This prevents a rejected field update from making the actor continue
serving an older view than its own journal. For an optimistic-concurrency result,
replay must reach the reported `actual` sequence before retrying. For any other
error, including a Turso event insert followed by a failing segment-metadata
update, replay determines whether the mutation committed before the error was
reported. If recovery cannot prove the complete journal tail, the handler fails
and actor supervision rebuilds state; startup replay is also strict, so neither
the old incarnation nor its replacement can serve known-stale state. The
event-store contract defines a read as the complete ordered tail or an error; a
detected partial response is never a successful replay. Deterministic truncation
and commit-then-report-storage faults therefore exercise the same fail-closed
path as production ambiguity.

Every PATCH/PUT request also receives one stable idempotency key before its first
append attempt: an HTTP caller's `Idempotency-Key` is preserved when supplied,
and the state layer generates one otherwise. The key is stored on the
field-update event and reused across actor and generation retries. If a backend
commits the event but the caller observes any ambiguous append result, strict
recovery finds that key in authoritative history and reports success without
appending a second field update. Repeat delivery with the same caller key is
therefore a no-op even after the first response was lost.

Deletion uses the same fail-closed rule. After any tombstone append error, strict
replay replaces the live actor state. A recovered durable tombstone is reported
as success; a definite pre-commit failure remains a failure with recovered live
authority, and an unreadable journal triggers supervision rather than serving
the pre-delete state.

The co-commit API carries explicit `reconcile_keys` and `reconcile_vectors`
flags. When the entity type declares keys, its candidate `key_rows` are the exact
current set: Postgres and the simulation store delete every prior row for that
entity before inserting the candidate rows. An empty set therefore purges stale
keys after PUT removes key properties or deletion tombstones the entity. Stores
that do not maintain the declared-key index continue to ignore both rows and the
reconciliation flag.

The state layer converts `EntityResponse { success: false }` into its public
error result before OData response mapping. A failed append is therefore a 5xx,
never an HTTP success containing an internally failed response.

After append success, the actor records the event in its bounded recent history
and runs the existing snapshot policy. Field writes consume the same bounded
replay budget as every other entity event.

### Sub-Decision 3: replay applies field updates in journal order

Hydration recognizes the reserved event, validates its versioned payload, and
applies the same PATCH/PUT helper used by the live path. The replayed update is
inserted into bounded recent history and advances the sequence number before the
next envelope is processed. Ordinary action and tombstone replay remain
unchanged.

Malformed field-update history is treated like other schema-incompatible
history: it is logged with the event identity and skipped while replay continues.

### Sub-Decision 4: every indexed journal writer uses the co-commit contract

Atomic composite appends carry each stream's exact declared key rows, vector
rows, and reconciliation flags. PostgreSQL and the deterministic store acquire
the shared fences for every affected projection partition in sorted order, then
co-commit all journals and exact row replacements. PostgreSQL holds all of a
batch's advisory locks in one dedicated-pool transaction, so a batch spanning
more types than the pool's connection count cannot deadlock while acquiring its
own fences. Turso co-commits vectors for a multi-stream batch inside its existing
immediate transaction; declared keys remain explicitly non-authoritative there.

The data-only create optimization declines keyed or vectored entity types, which
fall back to the projection-aware actor path. File initial writes derive rows
through the same helper as ordinary entity and composite writes and use the
co-commit API. These gates make the projection-watermark claim a property of
every production journal writer, rather than only the ordinary actor append.

### Sub-Decision 5: exactly reconcile legacy key and vector projections once

Pre-cutover tombstones may predate exact empty-set reconciliation and therefore
retain derived key or vector rows. The old backfill watermark cannot prove those
rows were purged: durable entity enumeration excludes some tombstones, while the
old resume path skipped any entity that already had a projection row.

Key and vector watermark signatures therefore carry a `v2|` reconciliation
schema prefix. A pre-cutover unversioned watermark never matches the current
signature, so keyed reads remain scan-safe and startup performs one exact pass.
The key signature encodes each key's name and ordered property list, not only
its name, because changing or reordering properties changes every canonical key
hash and must force another exact pass. Key-index hits and misses are both
trusted only under the current full signature; before it is current, a positive
row can be a stale tombstone or phantom holder and therefore also falls back to
the authoritative scan. Vector signatures likewise use a structured canonical
encoding of every declaration field so punctuation cannot create delimiter
collisions.

When a key or vector declaration disappears, its old durable watermark is
replaced under the same reconciliation fence with the canonical empty-set
signature. That explicit authority tombstone is required even though the
removed projection is no longer readable: entities may change while inline
maintenance is disabled, and a later identical re-add must compare unequal to
the empty marker and run a full exact reconciliation instead of reusing the
pre-removal watermark.
For each declared type, that pass unions durable entity IDs with IDs found in
the projection itself. Replay then replaces each entity's complete current row
set; deleted, phantom, unkeyable, or unembedded entities reconcile with an empty
set and purge their historical rows. Key reconciliation first derives every
desired row set, then purges all discovered holders before assigning any live
keys; this makes the result independent of whether a stale holder sorts before
or after the live owner.

That ordered, type-wide repair is protected by a storage-backed reconciliation
fence for `(tenant, entity_type)`. The worker acquires the exclusive fence before
reading the durable watermark and holds it through enumeration, replay, purge,
assignment, and the final watermark write. PostgreSQL implements the fence with
a transaction-scoped advisory lock from a small dedicated lock pool; live
key/vector appends take the matching shared advisory lock from that pool before
acquiring a general query connection. Lock waiters therefore cannot exhaust the
pool the exclusive worker needs to finish. The deterministic store uses the
write/read sides of a per-type async lock. A second worker therefore waits, re-reads the
watermark after the first worker commits it, and skips the now-current type; it
cannot act on cached legacy coverage. Live projection writes likewise finish
entirely before or after reconciliation and cannot be overwritten by replayed
older state.

Indexed readers take the shared side of that same per-type fence before reading
the authority watermark and retain it through the key lookup or vector scan.
They can therefore observe the complete old projection or the complete rebuilt
projection, never the destructive middle between purge and assignment. The
generation read side is acquired first, followed by the projection read side,
matching the writer lock order. PostgreSQL's shared reader is the advisory lock
used by live appends; Sim uses the deterministic read side; Turso uses a
clone-shared process-local read side under the coordinated-fleet rollout rule.

Turso's vector maintenance remains write-behind and therefore deliberately does
not claim a durable skip watermark. It reconciles vectors on every startup. Each
replacement carries the journal sequence from which its rows were derived; an
immediate database transaction compares that sequence with the current durable
journal head before replacing rows. A stale replay is skipped if a newer append
already won, while an append that starts later writes its newer rows after the
repair transaction commits. Exhausted write-behind retries can therefore be
repaired on the next startup instead of being hidden by an old completion marker.

The durable watermark read is definitive: a storage error is not interpreted as
an absent watermark and aborts before the first projection mutation. A type is
stamped with the new signature only when both enumerations, every
replay/replacement, and the watermark write succeed. An interrupted pass is
retried in full because skipping existing projection rows would preserve the
stale state this upgrade must remove.

Durable vector reads apply the same authority rule as keyed reads. PostgreSQL
and the deterministic store serve nearest-neighbor results only when the
durable watermark equals the full current vector-declaration signature;
missing, legacy, empty, or stale coverage returns `503 VectorIndexRebuilding`
instead of ranking incomplete rows. Turso does not claim durable vector
coverage and continues to rely on journal-sequence-fenced startup repair.

### Sub-Decision 6: hot spec generations fence every journal writer

The storage-backed per-type fence orders a projection-maintaining append against
one exact repair, but it cannot by itself stop this sequence: a writer snapshots
the old declaration set, a hot deploy swaps and repairs the new declarations,
then the delayed writer commits rows derived from the old set. In the
keyless-to-keyed case the old write would not even request a projection fence.

Every `ServerState` therefore owns one process-local asynchronous generation
`RwLock` plus a monotonic epoch per tenant:

- platform deploy, load-dir/inline, OS-app install and runtime repair, tenant
  bootstrap, and tenant deletion take the write side across storage/authority
  mutation, registry mutation, actor eviction, reaction refresh where
  applicable, and exact post-swap reconciliation;
- actor bootstrap, PATCH/PUT, deletion, File initial-content writes, and
  data-only creates take the read side before declaration-dependent commit work
  and retain it through the event journal commit;
- startup field/key/vector backfills take the read side across declaration
  capture and their complete projection pass. Strict deploy reconciliation calls
  the guarded internals while already holding the write side, avoiding recursive
  acquisition;
- state-machine and atomic-composite dispatches capture the epoch before every
  declaration-dependent metadata, guard, table, and preflight read. Actor-backed
  dependencies are resolved without the async read lock, then dispatch acquires
  the read side and compares the epoch. A mismatch discards all staged results
  and repeats from current declarations with the same idempotency key;
- the destination actor validates both that expected epoch and that its live
  table `Arc` remains the registry's current authority while holding its own
  read side through append. Entity-type removal evicts cached actors before the
  generation write side releases, so a retained external `ActorRef` cannot keep
  writing through a detached table; and
- lock order is always tenant generation first, then the storage projection
  fence. Composite staging resolves actor-backed dependencies before acquiring
  the fair generation lock. Nearest-neighbor reads likewise release both fair
  read guards after copying their coherent candidate set and before cold actor
  materialization, then reacquire the generation read side and fail closed if
  the staged epoch changed. These boundaries avoid nested-reader deadlocks when
  a generation writer or projection reconciler is queued.

The write guard publishes the next epoch before releasing its write lock. A
reader that sees the mutated registry can therefore never pair it with the
preceding epoch, and a reader whose staged work used the preceding epoch is
rejected before durable mutation.

The pre-swap step is deliberately limited to retiring authority for declarations
already absent from the installed generation. It does not require an exact
rebuild of declarations that are still present. A bad generation (for example,
one exposing duplicate durable keys) may fail its post-swap exact repair and
remain fail-closed, but a corrective generation can still remove or replace it.
Requiring the same failed rebuild before registry mutation would create a
remediation deadlock.

The lock is process-local because the live `SpecRegistry` mutation it protects
is process-local. The rollout contract therefore forbids processes serving the
same tenant under different registry generations: every old process is drained
before writes reopen on the new fleet. A future distributed hot-reload protocol
must add a durable generation token checked atomically by journal appends before
relaxing that coordinated-fleet rule.

### Sub-Decision 7: raw journal migration invalidates projection authority first

The Turso-to-PostgreSQL migration copies raw journal history rather than running
each event through a live declaration-aware writer. Before copying the first
event for a tenant, it atomically removes that tenant's key and vector backfill
watermarks in PostgreSQL. A crash can therefore leave retained projection rows,
but cannot leave a durable claim that those rows cover the newly copied history;
startup must run exact reconciliation before indexed reads become authoritative.

Multi-event appends also maintain event-segment metadata in the same transaction
or deterministic store mutation as their journal rows. Migration and composite
batches therefore preserve the same segment boundaries and replay source of
truth as single-event appends.

## Rollout Plan

The new envelope is forward-readable only by binaries containing this decision's
replay branch. Deployment is therefore a coordinated reader/writer cutover, not a
mixed-version rolling write:

1. **Pre-cutover** — keep the existing binary serving traffic. No field-update
   envelopes exist because the old writer cannot emit them.
2. **Cutover** — stop accepting writes and drain every old server process. Deploy
   the new binary to the full server fleet. Let its versioned key/vector
   reconciliation complete, then reopen writes only after every process reports
   the new revision healthy.
3. **Post-cutover** — new readers continue to understand all historical action
   and tombstone envelopes and additionally replay field-update envelopes. Once
   the first new envelope is written, do not roll back to a binary without this
   reader.

This arena PR remains unmerged and undeployed; the cutover applies when a
maintainer later chooses to ship it.

## Readiness Gates

- The field-update replay test passes from an empty journal and from history that
  begins with the existing `Created` event.
- PATCH merge and PUT replacement both survive actor replacement without a later
  state-machine action.
- Persistence failure is fail-closed and leaves the actor's live fields and
  sequence unchanged when no newer durable history exists, and the public OData
  request returns a server error.
- Retry exhaustion retains any unrelated durable history recovered after a
  concurrency conflict, including the writer that wins the final attempt,
  without publishing the rejected PATCH/PUT fields.
- Both commit-then-concurrency and commit-then-storage errors replay as success
  by the stable field-update idempotency key and do not append the PATCH/PUT
  twice; every other append error also replaces live state from strict replay
  before returning.
- A commit-then-storage error on deletion strictly recovers and publishes the
  durable tombstone, and a repeated delete remains idempotent.
- A failed authoritative retry read fails the actor message and supervision
  replays the journal before the actor serves another state response.
- PUT removal of declared-key properties purges the prior key row atomically in
  Postgres and the deterministic simulation store.
- An unversioned legacy watermark with tombstone and projection-only key/vector
  rows triggers an exact rebuild, purges those rows, and is replaced by the
  versioned signature only after the complete pass; a live replacement can then
  claim the released tombstone key.
- Two independent reconcilers sharing one store serialize on the per-type fence;
  the follower observes the current durable signature and cannot purge the
  leader's completed rows.
- A live projection-maintaining append remains pending while reconciliation owns
  the exclusive fence and commits atomically after the fence releases.
- An authoritative keyed read and nearest-neighbor scan remain pending during
  the exact purge/rebuild window and observe only the final projection after the
  exclusive fence releases.
- An indexed multi-stream batch remains pending behind the exclusive fence,
  atomically transfers keys, replaces vectors, and leaves every journal and row
  unchanged on a rejected claim.
- PostgreSQL lock waiters use a dedicated pool; an exclusive worker can complete
  repair even when the general query pool has a single connection; a batch that
  spans more partitions than the dedicated pool has connections still uses only
  one lock transaction and completes.
- Turso rejects vector replay derived from an older journal sequence and never
  persists a skip watermark for its write-behind projection.
- Durable nearest-neighbor reads return a rebuilding error until the full current
  vector signature is durably covered.
- An injected durable-watermark read failure leaves both projection rows and the
  existing watermark unchanged.
- A deterministically paused old-generation append blocks registry swap, commits
  first, and is included by the new generation's exact reconciliation.
- Tenant deletion waits for a deterministically paused old-generation append,
  then removes persistence, registry authority, and cached actors before the
  generation write side releases.
- A startup projection pass staged against the installed generation completes
  before a queued deploy can publish a new generation.
- Nearest-neighbor candidate capture releases generation and projection read
  guards before cold actor materialization; a queued writer completes and the
  read fails closed on the changed epoch instead of cycling behind fair locks.
- Declaration-dependent work staged against an older epoch is rejected without
  appending, and an actor retained across entity-type removal cannot write or be
  resurrected from the actor cache.
- A generation whose exact repair fails on duplicate live keys can still be
  replaced by a corrective declaration removal.
- Raw Turso-to-PostgreSQL journal migration invalidates both projection
  watermarks before its first copied event, so interruption remains fail-closed.
- The reserved event type cannot be dispatched as a domain action.
- The deployment procedure can drain old processes before reopening the OData
  write surface; mixed old/new readers are not permitted after writer enablement.
- Rollback tooling selects a revision containing the field-update reader after
  any new envelope has been committed.
- All simulation, workspace, strict-Clippy, review, and CI gates are green.

## Consequences

### Positive

- An acknowledged PATCH or PUT survives actor eviction and process restart.
- Journal order defines the result when field updates and actions both write an
  entity.
- Declared key/vector projections are committed from the same candidate fields.
- Composite and file-initial journal writes cannot bypass declared projection
  maintenance; indexed data-only creates take the full actor path.
- Legacy projection rows are discoverable and repaired during the coordinated
  reader/writer cutover before authoritative absence is restored.
- Hot spec mutation cannot overtake a local in-flight writer that derived its
  projection rows from the preceding declaration generation.
- Indexed reads cannot return false absence or stale nearest results from the
  destructive middle of an exact projection rebuild.
- Storage failure cannot leave a successful but non-durable in-memory write.

### Negative

- PATCH and PUT now pay one synchronous journal-append round trip.
- Field updates consume event and replay budgets, as durable mutations should.
- The first boot after this upgrade performs one full replay/reconciliation pass
  for each keyed or vectored entity type; interrupted passes restart in full.
- PostgreSQL uses a small additional lazy connection pool for advisory-lock
  transactions so lock waiters are isolated from ordinary query capacity.
- A live spec mutation waits for in-flight journal writers for that tenant to
  finish before swapping its registry generation.

### Risks

- A future change to the field-update payload must preserve versioned replay
  semantics. The reserved event type and decoder make that compatibility point
  explicit.
- Serving one tenant from mixed registry generations is unsupported; the
  coordinated fleet drain is part of correctness until a durable distributed
  generation token exists.

### DST Compliance

- Live application and replay share one deterministic field-update helper.
- Event timestamps and IDs continue to use `sim_now()` and `sim_uuid()`.
- No wall clock, random source, filesystem access, or unordered collection is
  introduced in the simulation-visible server path.
- A `temper-store-sim` regression test proves PATCH-only history survives actor
  replacement and deterministic replay.
- A deterministic integration regression proves an unversioned watermark cannot
  preserve tombstone or projection-only key/vector rows across upgrade.
- Deterministic overlap and watermark-read fault regressions prove the
  reconciliation fence prevents a current watermark from covering a later
  partial purge.
- A deterministic shared-reader regression proves indexed OData key resolution
  remains blocked through purge and observes the final rebuilt row set.
- A deterministic append rendezvous (no timing race or thread) proves the tenant
  generation barrier orders an old-generation journal writer before hot-swap
  reconciliation.
- Deterministic epoch and detached-actor regressions prove staged old-generation
  actions and removed entity types cannot append after authority changes.

## Non-Goals

- Changing OData authorization or request-body validation.
- Converting direct OData writes into user-declared automaton actions.
- Changing the snapshot interval or adding declared-key indexing to backends
  that do not already maintain it.

## Alternatives Considered

1. **Force a snapshot after every field update** — rejected because snapshots
   are derived acceleration state, not the ordered mutation log, and a snapshot
   failure would create a second durability protocol.
2. **Mutate live state, then append and roll back on failure** — rejected because
   candidate-state commit makes fail-closed behavior structural and ensures
   index rows are derived from the new fields without exposing speculative state.
3. **Encode field updates as a declared automaton action** — rejected because
   existing entity specs do not declare PATCH/PUT transitions, and a synthetic
   user action could collide with domain action names or guards.

## Rollback Policy

Before the first field-update event, rollback may restore the old binary. After
cutover, rollback must target a revision that retains the field-update replay
branch, even if it disables new writes. Existing events cannot be discarded or
served by an older reader without reintroducing acknowledged-write loss.
