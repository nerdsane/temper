# ADR-0171: Monotonic vector reconciliation

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
vector_set)` row. Before rebuilding a mismatched declaration set, the coordinator
atomically advances that type's generation, withdraws the prior completion watermark,
and receives the new token. Withdrawing the watermark prevents a coordinator for the
old signature from observing a now-invalid completion claim and skipping. Every entity
replacement and the final watermark write carry the token and fail if it is no longer
current. Live vector writes read the current type generation and co-commit it into the
entity fence with the new journal sequence. PostgreSQL takes a shared row lock for that
read: concurrent live writers remain independent, while a generation update waits for
all earlier writers to commit.

The in-process coordinator serializes snapshotting declarations and beginning a
generation so an older local invocation cannot obtain a later token after a newer
invocation. The durable generation remains the cross-process and crash boundary: once
another invocation advances it, any delayed entity replacement or watermark from the
older invocation is rejected. A stale generation is an explicit failure, not a
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
immediate transaction path and commit all three together. Non-vector single-event
appends retain their current optimized path. A durable outbox is not needed while all
affected records share this transactional boundary; a future backend with a physically
separate vector store must add a pre-commit durable obligation before it can advertise
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

## Rollout Plan

1. Pause vector-declaring writes and background vector backfill before the fleet
   cutover. Mixed old/new writers are unsafe because an old binary can still perform a
   sequence-less replacement that bypasses the new fence.
2. Add the Postgres version and reconciliation-generation tables, seeding legacy
   candidate sequences into generation zero. Add the equivalent idempotent Turso
   bootstrap DDL and deterministic simulation maps.
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
- An older overlapping declaration-set reconciliation cannot mutate rows or publish a
  watermark after a newer generation begins.
- Beginning a new generation atomically withdraws the previous completion claim, and
  an interrupted empty-set reconciliation remains discoverable without that watermark.
- Composite vector updates and deletes advance journal, rows, and the generation-plus-
  sequence fence atomically.
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
- Turso vector-declaring appends hold an immediate transaction through vector
  replacement instead of completing the index asynchronously.
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
