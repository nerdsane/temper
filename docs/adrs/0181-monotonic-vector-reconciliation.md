# ADR-0181: Monotonic vector reconciliation

- Status: Proposed
- Date: 2026-07-14
- Deciders: Temper core maintainers
- Related:
  - ADR-0155: Declared vector access path
  - ADR-0153: Declared composite-key index
  - ARN-216: Vector backfill races live writes and can permanently mark a stale index complete
  - ARN-201: Canonical append/projection transaction contract
  - `crates/temper-runtime/src/persistence/mod.rs`
  - `crates/temper-server/src/state/projection_backfill/vector_index.rs`
  - `crates/temper-store-{sim,postgres,turso}`

## Context

ADR-0155 made `entity_vector_index` derived state and gave each row a journal
`sequence_nr`, but the backfill contract does not carry the sequence of the state it
read. Postgres and Turso backfill therefore delete every row for an entity and insert
the replacement at sequence zero. The simulation store does not retain vector-index
versions at all.

That loses the ordering proof between a background rebuild and a live append. A
backfill can read state at sequence N, a live append can co-commit vectors for N+1, and
the delayed backfill can then replace N+1 with N. The backfill records its type
watermark after the stale replacement, so later starts skip the type and the stale
ranking becomes durable.

Row sequence numbers alone cannot close the race. A delete or cleared vector must
remove every candidate row. If the removed rows were the only place that retained
sequence N+1, delayed work from N could reinsert a vector after the purge. Correct
whole-entity replacement therefore needs an ordering record that survives an empty
row set.

Two existing optimizations compound the problem:

- A first-time backfill skips any entity that already has one vector row, even when
  that row represents an older journal sequence.
- An active entity with no currently usable vector is classified as skippable instead
  of reconciling an empty row set, so a failed earlier purge is not repaired.

Turso also acknowledges the journal append before its separate vector write-behind.
After retry exhaustion it logs the vector failure but returns append success. An
already-current watermark then prevents startup backfill from repairing that entity.

Two more journal-writing paths need the same ordering contract. Composite actions
append several streams atomically through `append_batch`; carrying only journal events
there would let those streams advance without advancing their vector fences. Spec
reconciliation can also overlap: sequence ordering alone cannot distinguish two
different declaration sets rebuilt from the same journal sequence, so an older rebuild
could replace a newer declaration set and then publish its stale watermark.

## Decision

### Sub-Decision 1: Fence whole-entity replacement with a durable sequence row

Every indexing backend will maintain one
`entity_vector_index_version (tenant, entity_type, entity_id,
reconciliation_generation, sequence_nr)` row per entity whose vector state has been
reconciled. The row is retained when the entity has no candidate vectors, including
deletion and cleared-vector purges.

`EventStore::backfill_entity_vectors` will accept the journal sequence observed by the
caller. In one backend transaction it will:

1. reject a stale reconciliation generation;
2. advance the entity's version row when its generation is newer, or when its
   generation matches and `observed_sequence >= sequence_nr`;
3. return success without changing candidates when a newer sequence is already stored;
4. delete all candidate rows for the entity;
5. insert the observed rows with `sequence_nr = observed_sequence`; and
6. commit the version fence and candidates together.

Equal-sequence replacement is allowed so replay is idempotent and can repair a
partially initialized derived index. A lower-sequence replacement is also a successful
outcome: the durable fence proves that the index already represents newer state.

**Why this approach**: vector declarations are reconciled as one post-transition
entity state, not as independent fields. One retained entity-level fence protects
model-tag changes, declaration removals, and empty purges without inventing sentinel
candidate rows.

### Sub-Decision 2: Order declaration-set reconciliation with a durable generation

Every authoritative indexing backend will maintain one
`entity_vector_reconciliation_generation (tenant, entity_type, generation,
declaration_revision, declaration_fingerprint, vector_set)` row. A caller supplies its
process-local monotonic tenant revision plus the fingerprint of the IOA source from
which it snapshotted declarations.

Postgres and Turso additionally maintain
`spec_declaration_authority (tenant, entity_type, revision, ioa_source,
declaration_fingerprint, present)`.
Database triggers advance this row in the same transaction as every IOA insert,
source change, and hard deletion **only when the affected catalog row is committed**.
PlatformStore staging deliberately writes `committed = false`; staging and discarded
uncommitted rows neither fence the still-published declaration nor withdraw its
watermark. The false-to-true commit transition is the publication point that advances
authority. The row is a tombstone when `present = false`, so its revision survives
delete/re-add and process restart. A committed spec mutation also advances an existing
reconciliation generation and withdraws its watermark immediately; stale work is
fenced at the declaration commit point, not only after the next coordinator starts.

Persistent reconciliation uses the catalog's stored content fingerprint, falling back
to hashing authoritative IOA bytes only for migrated rows, or uses the fixed
`absent:v1` tombstone fingerprint. Validation and the journal/index mutation hold the
same authority-row barrier through commit. A truly empty compatibility store may
atomically accept its first fingerprint as authority only when neither a catalog row
nor an authority/tombstone row exists. That bootstrap never overwrites catalog truth,
and concurrent different first writers leave exactly one winner. A replica holding A
therefore cannot begin after durable B merely because its call arrives later, and an
intentional A re-add receives a strictly newer tombstone-preserved revision. The
process-local revision remains diagnostic input and is not trusted as cross-process
authority.

Deterministic simulation mirrors the separate durable authority map. Declaration
changes use `persist_spec_declaration`; once an authority entry exists, no caller-local
revision can replace its fingerprint. Direct EventStore tests retain an empty-store
first-writer bootstrap, but append validation stages that bootstrap and publishes it
only if the complete append/batch commits. Retrying the identical declaration and
vector set reuses its generation and does not withdraw an already-valid watermark.

Before rebuilding a mismatched declaration set, the coordinator atomically advances
that type's generation, withdraws the prior completion watermark, and receives the new
token. Withdrawing the watermark prevents a coordinator for the old signature from
observing a now-invalid completion claim and skipping. Every entity replacement and
the final watermark write carry the token and fail if it is no longer current. Live
vector writes read the current type generation and co-commit it into the entity fence
with the new journal sequence. PostgreSQL takes a shared row lock for that read:
concurrent live writers remain independent, while a generation update waits for all
earlier writers to commit.

The in-process coordinator serializes only declaration snapshotting and durable
generation allocation. It releases that lock before journal enumeration, replay, and
row replacement, so a long rebuild does not globally serialize unrelated tenants or
types. The durable declaration revision and generation remain the cross-process and
crash boundary. A stale revision or generation is an explicit failure, not a
successful no-op, because it must prevent the stale invocation from claiming
completion.

**Why this approach**: equal-sequence replay is necessary for idempotent repair inside
one declaration set, so sequence alone cannot order two different sets. A durable
type-level epoch makes that order explicit without coupling persistence backends to
the in-memory registry implementation.

### Sub-Decision 3: Backfill every entity from an observed journal sequence

State recovery will return both fields and `EntityState::sequence_nr`; deleted and
phantom outcomes will also retain the recovered sequence used for an ordered purge.
Vector repair will enumerate durable journal stream IDs, including streams whose
latest state is `Deleted`, through a repair-specific EventStore method. It will not
reuse active-entity listing, whose Postgres and Turso implementations intentionally
exclude deleted streams.

Whenever the watermark is absent or its signature differs, the backfill will load and
reconcile every journal stream for the declared type. It will not skip an entity merely
because some candidate row already exists. Deleted streams reconcile an empty row set
at their deletion sequence, which both removes a legacy stale candidate and leaves the
version tombstone that rejects older work.

An active entity with no valid vector/model pair will reconcile an empty row set at
its observed sequence. This repairs stale candidates left by an interrupted or older
write path.

The watermark signature gains a reconciliation-protocol revision. Existing ADR-0155
watermarks therefore mismatch once after rollout and force a sequence-aware rebuild
without relying on backend-specific migration state.

The work set is the union of types with current vector declarations, types with a
stored vector-backfill watermark, and types with any durable reconciliation state
(generation rows, retained entity fences, or candidate rows). The third source is
required because beginning a generation withdraws the old watermark: if the process
crashes while reconciling an empty declaration set, the durable generation still makes
the purge discoverable on restart. It also covers generation-zero rows created by live
writes or migrated from ADR-0155 before their first formal reconciliation. A previously
covered type whose current declaration set is empty is rebuilt to an empty candidate
set across all of its journal streams and then receives the revisioned empty-set
watermark. Removing the final declaration therefore cannot leave an old watermark that
would match if the identical declaration is later re-added.

**Why this approach**: the supported exact-scan design is explicitly bounded to about
1,000 entities per tenant. Re-reading the full type after an incomplete run is simpler
and sounder than a row-presence shortcut that cannot prove journal freshness.

### Sub-Decision 4: Co-commit live vector state on every journal-writing path

Postgres and the simulation store will update the version fence in the same critical
section or transaction that already commits the journal and candidate rows.

The composite `append_batch` contract will carry each stream's complete
post-transition vector rows plus whether its declared vector set must be reconciled.
Each backend will co-commit those rows and the current reconciliation-generation fence
with every batch journal append. Empty rows are meaningful: a composite delete or
cleared vector purges candidates while retaining the fence. Backends without vector
index authority may still commit the journal batch, but cannot later advertise a
vector-reconciliation watermark.

Turso will stop using event-first vector write-behind. Its journal, version fence, and
vector tables share the same libSQL database, so an indexed append will use the existing
immediate transaction path and commit all three together. Every spec-derived writer,
including a currently non-vector declaration, carries the fingerprint of the exact
transition-table snapshot that produced its event. The store validates that fingerprint
before any journal mutation. This prevents an old replica from advancing the journal
after a newer declaration adds, removes, or changes vectors. The actor retry path,
composite staging, native data-only create, and atomic File initial-write path all retain
their original table snapshot through commit; none re-read a hot-swapped table merely
to label old semantics with a new fingerprint.

The single-event optimization remains available only to legacy/untyped appends that
carry no declaration fingerprint. A durable outbox is not needed while all affected
records share this transactional boundary; a future backend with a physically separate
vector store must add a pre-commit durable obligation before it can advertise
vector-index authority.

**Why this approach**: an outbox would add a second state machine, cleanup rules, and
watermark coupling to emulate atomicity that the current Turso topology already
provides. The longer indexed-append transaction is the deliberate durability cost.

### Sub-Decision 5: A watermark is a persisted convergence claim

A type is reported complete only when every entity load and ordered replacement
succeeds and the generation-checked watermark write itself succeeds. A lower-sequence
replacement rejected within the current generation counts as converged because newer
durable vector state is present. A stale-generation replacement does not.

Failure to persist the watermark logs a failure outcome; the code must not emit the
"type watermarked" completion event. The next run replays the bounded type and
converges idempotently.

The coordinator must cross the declaration barrier before trusting an existing
watermark, then re-read completion under its short coordinator lock. This closes the
window where a spec mutation withdraws a completion claim after the coordinator's
initial tenant-wide read but before it decides to skip the type.

### Sub-Decision 6: Full spec replacement persists omission tombstones

For full-directory replacement, the durable spec catalog and in-memory registry form
one ordered publication. Omission discovery is part of the backend write transaction,
not a query performed before mutation. Postgres takes a tenant-scoped advisory
transaction lock; Turso begins an immediate transaction. Only after that shared lock is
held does the backend read the current catalog and present declaration authority,
upsert the incoming committed set, tombstone every omission, and update tenant
constraints. The server hot-load path and CLI startup overlay both call this exact
primitive. Concurrent replicas therefore serialize as two complete replacements; they
cannot each observe an empty catalog and commit their union. Merge-mode inline
submissions do not delete omitted types.

The transaction returns the exact durable omissions it replaced. The server unions
those with any registry-only compatibility omissions before publishing the new
registry. Turso commits only the addressed tenant's incoming set and constraints; it
does not use a process-wide commit of unrelated staged rows.

Startup can subsequently merge built-in agent entities into an app tenant. That phase
must publish their sources, exact fingerprints, and verification state through the
tenant's active `PlatformStore`, including shared Postgres. A Turso-only bootstrap
accessor would leave the in-memory built-ins advertised while replacement tombstones
continued to fence every Postgres writer. Each verified built-in is committed by
tenant and entity type; bootstrap must never use a tenant-wide commit that could
promote an unrelated app declaration still undergoing verification on another
Postgres replica. Verification status and commitment are finalized in one
fingerprint-checked store operation, so a same-type overwrite by another replica
fails closed instead of publishing bytes that the current bootstrap did not verify.

A delete always leaves authority at `absent:v1`, even when compatibility first-writer
bootstrap created authority without a `specs` row. The deletion trigger/transaction
advances any existing reconciliation generation and removes its watermark. The absent
type therefore remains discoverable from durable reconciliation state after a crash,
can purge retained candidates without loading a current transition table, and cannot
be resurrected by stale writers or startup restore.

**Why this approach**: removing a type only from the process registry is not a durable
declaration change. On restart the old catalog row would restore the type, while an
old vector watermark could suppress its purge. Ordering storage before registry
publication fails closed during hot swap and makes deletion replayable.

### Sub-Decision 7: Registry publication preserves only live actor incarnations

Before durable catalog mutation, the server snapshots the actor key, task UUID, and
ready supervised-incarnation epoch only for matching actors that completed
`pre_start`. The `ActorRef` exposes readiness and a monotonically increasing
`pre_start` epoch in one packed atomic value. A drop guard in the actor run future
clears readiness on normal shutdown, handler panic, and task cancellation; every
supervised restart advances the epoch even though it reuses the task UUID. After the
durable commit and while holding the actor/spec publication write lock, the server
preserves an actor only when the same key still maps to the same UUID and ready epoch.
It stops and removes actors created or restarted during the publication gap, same-key
replacement incarnations, unready or dead actors, every removed type, and every legacy
fallback actor on a tenant's first registry publication.

Preserved actors share the registry transition-table lock and hot-swap in place. An
actor that captured the old declaration after the snapshot cannot survive publication
merely because its map key matches, and an unwind cannot leave a dead actor falsely
advertised as ready.

### Sub-Decision 8: Replay evidence fails closed

Vector reconciliation treats strict journal recovery as evidence, not a best-effort
read. A malformed persistence envelope is propagated instead of being classified as
an empty or phantom entity, and an injected truncated-read fault returns an error
instead of a successful prefix. Any load, replay, replacement, or watermark failure
keeps the completion claim absent so restart retries the complete bounded type.

## Rollout Plan

1. Pause vector-declaring writes and background vector backfill before the fleet
   cutover. Mixed old/new writers are unsafe because an old binary can still perform a
   sequence-less replacement that bypasses the new fence.
2. Add the Postgres version, reconciliation-generation, and declaration-authority
   tables, including spec-mutation triggers and deletion tombstones. Seed legacy
   candidate sequences into generation zero and authority tombstones for vector state
   whose spec is already absent. Apply tenant RLS to all new Postgres metadata. Add the
   equivalent idempotent Turso bootstrap DDL and deterministic simulation maps.
3. Add tombstone-inclusive journal-stream enumeration for vector repair without
   changing active entity-listing semantics.
4. Deploy the generation-and-sequence-carrying trait and backend implementations to
   every single and composite writer before resuming background work. The
   protocol-revised watermark signature forces one complete ordered rebuild for every
   currently or previously declared vector type, including deleted streams and empty
   current declaration sets.
5. Confirm the revisioned rebuild and watermark persistence, then resume
   vector-declaring writes. Keep the new version table on rollback; it is additive
   derived state and protects later re-deployment.

## Readiness Gates

- A deterministic stale-backfill/live-write schedule preserves the live vector.
- A newer empty purge cannot be followed by stale vector resurrection.
- A pre-rollout stale candidate belonging to a deleted journal stream is purged and
  fenced at the deletion sequence during the revisioned rebuild.
- Remove-all declarations purge and fence the type; intervening writes followed by
  re-adding the identical declaration signature trigger a fresh rebuild.
- An older overlapping declaration-set reconciliation cannot obtain a generation,
  mutate rows, or publish a watermark after a newer durable declaration completes.
- A delete interrupted after generation allocation resumes after restart, and an
  identical later re-add obtains a newer authority revision in both durable stores.
- Beginning a new generation atomically withdraws the previous completion claim, and
  an interrupted empty-set reconciliation remains discoverable without that watermark.
- Composite vector updates and deletes advance journal, rows, and the generation-plus-
  sequence fence atomically.
- A stale non-vector writer cannot advance the journal after a newer vector declaration
  becomes authoritative, in either a single append or an otherwise-valid batch.
- Full replacement persists omission tombstones before registry publication; restart
  cannot restore a removed type, and compatibility authority without a catalog row is
  still tombstoned.
- Concurrent Postgres and Turso replicas replacing an empty tenant with disjoint
  catalogs leave exactly one complete catalog after reopen, never their union.
- An actor that panics or is cancelled after `pre_start` immediately loses readiness;
  publication never preserves its dead incarnation.
- Malformed or truncated journal recovery cannot publish a vector completion
  watermark from a successful prefix.
- A fresh compatibility store establishes exactly one first-writer declaration and
  thereafter obeys the same durable fence as catalog-backed stores.
- Deployment automation prevents sequence-less old writers/backfills from overlapping
  sequence-fenced writers during the cutover.
- Equal-sequence replay is idempotent.
- Postgres and Turso integration tests exercise the same ordering contract as the
  simulation store.
- A Turso indexed append is atomic under an injected journal/index transaction failure.
- Backfill cannot report or persist completion after a load, replacement, or watermark
  failure.
- The full workspace, strict Clippy, determinism, and live local server flows pass.

## Consequences

### Positive

- Vector candidates become monotonic with the journal across live writes, backfill,
  replay, deletion, and cleared vectors.
- Turso no longer acknowledges an event while silently dropping its vector update.
- A watermark means the sequence-aware reconciliation actually converged.

### Negative

- Indexing backends store one additional small row per reconciled entity.
- Turso spec-derived appends hold an immediate transaction through declaration
  validation and, when applicable, vector replacement instead of completing the index
  asynchronously.
- An incomplete or revised backfill re-reads the bounded entity type instead of
  resuming from row presence.

### Risks

- The longer Turso transaction may increase contention. Existing bounded write gates,
  append timeouts, and transaction retries remain in force; non-vector appends keep the
  optimized single-event path.
- A future backend could incorrectly inherit no-op vector methods. Such a backend must
  continue returning no vector watermark authority until it implements this contract.

### DST Compliance

- The simulation fence is a `BTreeMap` updated under the same existing store lock as
  journal and candidate state.
- Race coverage is an explicit deterministic event order: observe N, commit N+1,
  attempt N. It needs no thread, wall clock, or random scheduling.
- The change introduces no ambient I/O, nondeterministic collection, or unbounded
  mailbox behavior in simulation-visible crates.

## Non-Goals

- Unifying the parallel store implementations; ARN-201 owns that broader contract.
- Producing embedding values, approximate-nearest-neighbor indexes, or ranking changes.
- Making non-indexing EventStore backends authoritative for vector queries.

## Alternatives Considered

1. **Compare only candidate-row sequence numbers** — rejected because a newer empty
   purge leaves no row that can reject delayed insertion.
2. **Serialize backfill and live writes in the server** — rejected because process
   locks do not survive restart and cannot protect independent writers.
3. **Serialize declaration-set backfills only in memory** — rejected as the sole
   mechanism because it cannot reject delayed work from another process or a crashed
   predecessor. A small local lock is still used to order declaration snapshotting and
   generation allocation; the durable generation is authoritative.
4. **Keep Turso write-behind with retry-only recovery** — rejected because retry
   exhaustion and a current watermark can make loss permanent.
5. **Add a Turso dirty-row/outbox workflow** — rejected for the current topology
   because journal and vector tables already share one transaction manager. It becomes
   mandatory if a future backend cannot co-commit them.

## Rollback Policy

The version table is additive and may remain populated. Rolling the binary back to an
implementation that performs sequence-less replacement would reopen ARN-216, so a
binary rollback must first pause vector backfill and vector-declaring writes. After
restoring this implementation, rerun the revisioned backfill before resuming traffic.
