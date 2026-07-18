# ADR-0171: Exact Declared-Key Reconciliation

- Status: Proposed
- Date: 2026-07-13
- Deciders: Temper core maintainers
- Related:
  - ADR-0153: Declared composite-key index
  - ADR-0155: Declared vector access path
  - ARN-238: Stale declared-key ownership after delete or key removal
  - `crates/temper-runtime/src/persistence/mod.rs`
  - `crates/temper-server/src/entity_actor/actor.rs`
  - `crates/temper-server/src/odata/query_plane_read/mod.rs`
  - `crates/temper-server/src/odata/read_support.rs`
  - `crates/temper-server/src/state/projection_backfill/key_index.rs`
  - `crates/temper-store-{postgres,sim}`

## Context

ADR-0153 made a declared key row part of the journal append, but its persistence
contract carries only the rows that currently exist. Stores delete an entity's old
row only for each key name present in that emitted set. An empty set is therefore
ambiguous: it can mean either "this caller is not maintaining keys" or "this keyed
entity now owns no keys."

That ambiguity leaves durable stale ownership in two normal state transitions:

- a soft-deleted entity retains its fields and the actor continues to emit its key;
- an all-null or non-scalar key produces no row, so the store never sees a key name
  whose prior row should be removed.

The stale row can reject a later entity that legitimately reclaims the value and can
resolve a keyed read to an entity that no longer owns the key. Because the backfill
currently upserts only non-empty rows, skips deleted entities, and trusts an existing
same-key-set watermark, it cannot repair rows written before this correction.

ADR-0155 already establishes the correct derived-index shape for vectors: the caller
states whether it owns reconciliation, and a participating store replaces the
entity's complete current row set even when that set is empty. Declared keys need the
same exact-set semantics while preserving their stronger uniqueness and transaction
requirements.

## Decision

### The append contract distinguishes reconciliation from row presence

`append_with_index_rows` gains a `keys` signal in the typed `IndexReconciliation`
policy alongside `vectors`. The entity actor sets it for a type with declared keys
and passes the complete current key-row set. A tombstone emits no key rows. An
all-null or otherwise unkeyable declaration also emits no row, but the signal remains
true.

`append_with_keys` is an exact-key-set convenience API and therefore enables key
reconciliation even for an empty row set. Plain `append` keeps reconciliation off.

`PersistenceAppend`, the per-stream input to atomic `append_batch`, carries the
same complete `key_rows` plus an explicit `reconcile_keys` signal. Composite action
dispatch derives those rows from every touched stream's final post-batch state, not
from an intermediate sub-write. A keyed stream that ends deleted or unkeyable
therefore participates with an authoritative empty set.

**Why this approach**: row presence cannot encode an empty authoritative set. A
separate signal makes the ownership boundary explicit without inventing sentinel key
rows or leaking spec declarations into stores.

Every durable write surface supplies the versioned key-contract signature, including
the versioned empty signature for a type with no declared keys. This includes normal
actor actions, HTTP PATCH/PUT field updates, data-only creates, composite batches, and
the synthetic File initial-content path. PATCH/PUT is represented by a private,
versioned journal payload that is recognized only by both its internal event type and
schema marker; a spec action may use the same event-type string and still replays as a
normal `EntityEvent`. Field updates consume the same bounded replay-tail budget as
actions and, after an optimistic-concurrency loss, rebuild from a clean authoritative
state before retrying. They never replay the journal onto speculative state.
Because the tombstone is terminal and replay stops at it, field updates reject a
locally known `Deleted` state and re-check that boundary after concurrency catch-up;
they never append an unreplayable field-update suffix after a concurrent delete.

**Why this approach**: key ownership is a property of every durable state mutation,
not only spec action dispatch. A synthetic or field-only write that bypasses the
co-commit contract can recreate stale ownership immediately after a successful repair.

### Participating stores replace the entity's key set atomically

Postgres validates every normal-append claim, appends the journal event, deletes
every prior key row for `(tenant, entity_type, entity_id)`, inserts the complete
current set, and commits once. For a multi-stream batch it locks and validates every
stream, removes every participating entity's old rows, installs all final key sets,
then inserts all journal events in the same transaction. Removing all participants
first permits an atomic ownership transfer regardless of sub-write order. A
validation failure, storage failure, optimistic-concurrency failure, unique conflict,
or insert race rolls back every journal and key mutation.

The simulation store performs the same normal and batch contracts under its existing
deterministic lock. For a batch it constructs and validates a complete next key map
before mutating any journal; only then do every journal and the exact key map change
together. This keeps transfer, failure, retry, and replay behavior aligned with
Postgres.

Turso remains deliberately non-authoritative for declared keys under ADR-0153: it
does not co-commit them and ignores the new signal. Making it authoritative requires
its separate backend-parity work, not a write-behind uniqueness approximation.
The store contract exposes this capability explicitly; the server neither runs key
repair nor loads/caches coverage watermarks for a backend that reports false, and it
does not trust that backend's legacy key rows as keyed hits or misses.

**Why this approach**: deleting by entity identity expresses the full declared-key
namespace and also cleans rows for declarations that changed. Per-emitted-key deletes
cannot release an empty set. Co-commit is required because uniqueness is domain state,
not a best-effort projection.

### Backfill uses the same exact reconciliation primitive

`backfill_entity_keys` is defined as type-contract- and sequence-fenced exact
reconciliation: the caller passes the key-set signature and monotonic contract
revision captured before replay, plus the journal sequence that produced the replayed
state. Only while both fences still hold does the store delete all existing rows for
the entity and insert the supplied current rows, including an empty set. Postgres
acquires the tenant/type contract lock before the per-stream advisory transaction
lock, validates signature and revision in the same transaction, then validates the
stream sequence before mutation. Sim performs both checks under its deterministic
store lock. A live write on either the same stream or a different stream of the type
therefore makes a stale repair fail instead of letting stale state overwrite the
newer key set. The next full pass retries from current state.

A current claim already held by another live stream is not a skippable row. Both
authoritative stores reject the repair before mutation, the caller records the entity
as failed, and the type remains unwatermarked. Postgres also lets a unique-constraint
race fail the transaction instead of using conflict-ignore insertion. Historical
duplicate durable state therefore stays scan-safe and visible for operator repair;
it can never be silently certified as a complete ownership index.

The backfill calls the exact primitive for current entities whose key is
all-null/non-scalar and for definitively skippable deleted/phantom streams, so stale
ownership is removed instead of silently retained. Any type without an exactly
matching coverage signature is replayed in full. Existing row presence is not used as
a resumability checkpoint: an interrupted pre-v3 pass can leave a stale row without a
watermark, and trusting that row would incorrectly certify the incomplete index.

The persisted coverage signature includes a derivation-contract version. Advancing
that version in this release makes every previously watermarked keyed type fail the
exact-match gate once, forcing a complete re-key under the corrected semantics. The
new watermark is written only after every entity is keyed, reconciled empty, or
definitively skippable without a load failure.

Coverage publication is also fenced by a per-tenant/type monotonic contract revision,
stored by Postgres in `key_index_contract_state` (migration 0013) and modeled in Sim.
Before replay begins, backfill establishes its target signature under the type lock;
this invalidates any older watermark and returns the revision to fence. Every live
write atomically reconciles its own signature with that state. A different signature
advances the revision and removes coverage, including remove/re-add and A → B → A
cycles. Every per-entity repair and final publication succeeds only when both the
revision and target signature still match. Establishing the target before replay
matters: otherwise a live write using the old signature could occur during a
new-signature repair without changing the previously captured revision. Fencing each
row mutation also matters: rejecting only final publication cannot roll back stale
rows already committed earlier in the pass after another entity advanced the type
contract.

**Why this approach**: a backend-specific data migration cannot reconstruct current
event-sourced state safely. The existing authoritative backfill already can; a
versioned coverage signature turns semantic changes to that derivation into an
explicit, repeatable projection migration.

### Keyed reads consume authoritative ownership before asynchronous projections

A filter that exactly covers a declared key consults the co-committed key index only
after the current v3 coverage signature is complete. At that point both hits and
misses bound the candidate set to zero or one, before any asynchronous native
projection is considered. `$count` and `$orderby` remain correct over that same
bounded set. The bounded hit is materialized from journal/actor state rather than a
possibly stale catalog row.

Coverage validation and key lookup are separate backend reads, so the read path
captures the monotonic contract revision before validating coverage and checks it
again after lookup. Any revision change means an incompatible live contract write
crossed the proof; the lookup is discarded and the request falls back to the
journal-backed authoritative scan. Composite-key URL resolution uses the same
capability, coverage, and revision fence before accepting an indexed hit.

Before the matching v3 watermark, neither a hit nor a miss is authoritative. An old
hit can name an entity whose delete committed before exact reconciliation while its
pre-delete projection removal was crash-lost. The planner therefore bypasses native
and catalog materialization for every recognized exact-key query and performs the
existing budgeted authoritative scan. A large incomplete type may return the honest
413 until repair completes; it never returns a durable tombstone as live. The native
projection remains valid for non-keyed queries. Materialization treats a replayed
`Deleted` status as absent and schedules the existing idempotent projection removal
as repair.

**Why this approach**: the key index and journal change in one transaction, while the
query projection intentionally converges later. A keyed read must therefore give the
stronger source precedence. Filtering tombstones at the materialization boundary
keeps all read modes from returning an entity whose durable lifecycle says it is
deleted, including count-bearing queries that cannot use the native fast path.

## Rollout Plan

Ship the normal/composite contract, store implementations, actor behavior, versioned
backfill repair, and regression coverage in one release. On startup or spec
reconciliation, an old coverage signature triggers one full pass per keyed type.
Until that pass completes, the read plane refuses to treat any keyed hit or miss as
authoritative and uses budgeted journal/actor materialization.

## Readiness Gates

- Seeded actor/store DST proves delete and all-null release ownership across restart.
- Composite delete/reclaim, partial-null, and rename cases prove exact final-row
  replacement and atomic ownership transfer.
- Fault, concurrent reclaim, same-stream and cross-stream stale-backfill fencing,
  retry, and replay cases prove journal/key atomicity.
- A target-contract race and A → no-key → A cycle prove old signatures cannot regain
  coverage through an ABA-equivalent watermark.
- Real Postgres coverage proves empty reconciliation, rollback, concurrent reclaim,
  sequence-fenced repair behavior, target-contract revision fencing, and
  conflict-before-watermark failure.
- PATCH/PUT replay, real intervening-journal retry, concurrent-delete rejection,
  replay-budget, action-name collision, data-only create, and synthetic File
  initial-content regressions cover every non-composite write surface.
- A live local server flow demonstrates release and reclaim through the actual API.
- Restarted normal, count-bearing, and ordered keyed reads return only the current
  owner while a lagging tombstone projection is repaired; the same options never
  return a crash-lost pre-v3 tombstone.
- A forced coverage-A / live-write-B / lookup interleaving changes the contract
  revision and proves the read falls back to replay instead of certifying B's miss
  under A.
- A non-authoritative Turso backfill cannot populate the server's coverage cache.
- Full repository gates, dedicated PR-diff review, Greptile, and CI are clean.

## Consequences

### Positive

- Key ownership exactly follows current non-deleted entity state.
- Empty current sets are first-class and no longer indistinguishable from no-op calls.
- Existing stale rows are repaired automatically from authoritative state.
- Store and backfill behavior share one durable reconciliation contract.
- Normal actor writes and composite batches share that contract.
- Keyed query results cannot be widened by a lagging projection or a replayed
  tombstone after restart.

### Negative

- Each write for a keyed entity performs one reverse-index delete before inserting
  its current key rows.
- The first deployment performs one complete repair pass for each keyed type without
  a matching v3 watermark, including interrupted/unwatermarked indexes.
- Per-stream Postgres appends take an advisory transaction lock so a background repair
  and a live write cannot cross after the replay-sequence check.
- Authoritative Postgres writes also take a tenant/type contract lock before their
  stream lock; backfill target establishment, every repair-row transaction, and final
  publication take that same contract lock.
- Historical duplicate live claims prevent the keyed type from becoming authoritative
  until the invalid durable data is repaired.
- Before v3 repair completes, recognized key queries may require a bounded full-type
  replay and return 413 when that proof exceeds the configured budget.

### Risks

- A reconciliation implementation that publishes deletions before all final claims
  validate could release a valid old key on a rejected write. Normal Postgres appends
  validate first; composite transfers keep all deletes/inserts/journals in one
  transaction, while Sim validates a cloned next map before any mutation.
- Trusting the index during the repair would make an unremoved stale or missing row
  authoritative. Every incomplete type is fully replayed, and the versioned watermark
  withholds authoritative hits and misses until the pass succeeds.
- Replaying state and repairing later without a sequence fence could restore an old
  key after a concurrent rename/delete. The expected sequence plus shared stream lock
  makes either the repair or the live append happen first; a stale repair is rejected.
- Capturing a revision before declaring the repair target could miss a live write that
  still uses the old contract. Backfill establishes the target first, and publication
  compares both target signature and revision.
- Consulting the asynchronous projection before the exact key index could return both
  a deleted former owner and its replacement. Declared-key filters resolve bounded
  candidates first for every query option, and materialization excludes any replayed
  tombstone.
- Treating no-op store defaults as successful repair could grant a non-maintaining
  backend false authority. An explicit backend capability gates repair, watermark
  caching, and keyed resolution.

### DST Compliance

- The actor derives rows only from the transition table and post-event state.
- The sim store continues to use its deterministic lock and ordered collections;
  no clock, randomness, ambient I/O, or thread is added.
- Seeded tests cover faults, retry, restart, and ownership reclaim with `sim_now()`
  and `sim_uuid()`.

## Non-Goals

- Making Turso's declared-key index authoritative.
- Changing declared-key hashing or non-keyed OData query semantics.
- Merging or deploying an arena PR before adjudication.

## Alternatives Considered

1. **Emit sentinel rows for deleted or null keys** — rejected because sentinels would
   participate in uniqueness and make storage semantics depend on invented hashes.
2. **Pass only declared key names to stores** — rejected because a store can replace
   all rows by entity identity more simply, including rows from removed declarations.
3. **Delete stale rows asynchronously after commit** — rejected because a reclaim can
   race the cleanup and journal/key ownership would diverge on failure.
4. **Run a SQL-only cleanup migration** — rejected because SQL rows do not contain the
   authoritative post-replay entity state needed to distinguish valid from stale keys.

## Rollback Policy

Reverting the append signal and store replacement behavior is source-compatible only
after the repair pass has completed, but would reintroduce the stale-ownership bug on
future deletes/nulls. The coverage version may remain advanced; the key index is
derived state and can always be rebuilt from journaled entity state.
