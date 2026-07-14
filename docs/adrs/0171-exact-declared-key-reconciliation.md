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

`append_with_index_rows` gains a `reconcile_keys` signal alongside
`reconcile_vectors`. The entity actor sets it for a type with declared keys and passes
the complete current key-row set. A tombstone emits no key rows. An all-null or
otherwise unkeyable declaration also emits no row, but the signal remains true.

`append_with_keys` is an exact-key-set convenience API and therefore enables key
reconciliation even for an empty row set. Plain `append` keeps reconciliation off.

**Why this approach**: row presence cannot encode an empty authoritative set. A
separate signal makes the ownership boundary explicit without inventing sentinel key
rows or leaking spec declarations into stores.

### Participating stores replace the entity's key set atomically

Postgres validates every new claim, appends the journal event, deletes every prior
key row for `(tenant, entity_type, entity_id)`, inserts the complete current set, and
commits once. A validation failure, injected/storage failure, optimistic-concurrency
failure, or insert race rolls back both journal and key ownership.

The simulation store performs the same ordering under its existing deterministic
lock: faults and conflicts are decided before mutation; then the journal and exact
key set change together. This keeps failure/retry/replay behavior aligned with
Postgres.

Turso remains deliberately non-authoritative for declared keys under ADR-0153: it
does not co-commit them and ignores the new signal. Making it authoritative requires
its separate backend-parity work, not a write-behind uniqueness approximation.

**Why this approach**: deleting by entity identity expresses the full declared-key
namespace and also cleans rows for declarations that changed. Per-emitted-key deletes
cannot release an empty set. Co-commit is required because uniqueness is domain state,
not a best-effort projection.

### Backfill uses the same exact reconciliation primitive

`backfill_entity_keys` is defined as exact reconciliation: delete all existing rows
for the entity and insert the supplied current rows, including an empty set. The
backfill calls it for current entities whose key is all-null/non-scalar and for
definitively skippable deleted/phantom streams, so stale ownership is removed instead
of silently retained.

The persisted coverage signature includes a derivation-contract version. Advancing
that version in this release makes every previously watermarked keyed type fail the
exact-match gate once, forcing a complete re-key under the corrected semantics. The
new watermark is written only after every entity is keyed, reconciled empty, or
definitively skippable without a load failure.

**Why this approach**: a backend-specific data migration cannot reconstruct current
event-sourced state safely. The existing authoritative backfill already can; a
versioned coverage signature turns semantic changes to that derivation into an
explicit, repeatable projection migration.

## Rollout Plan

Ship the contract, store implementations, actor behavior, versioned backfill repair,
and regression coverage in one release. On startup or spec reconciliation, an old
coverage signature triggers one full pass per keyed type. Until that pass completes,
the read plane refuses to treat a keyed miss as authoritative and retains its safe
fallback behavior.

## Readiness Gates

- Seeded actor/store DST proves delete and all-null release ownership.
- Composite partial-null and rename cases prove exact current-row replacement.
- Fault, concurrency, retry, and replay cases prove journal/key atomicity.
- Real Postgres coverage proves empty reconciliation and rollback behavior.
- A live local server flow demonstrates release and reclaim through the actual API.
- Full repository gates, dedicated PR-diff review, Greptile, and CI are clean.

## Consequences

### Positive

- Key ownership exactly follows current non-deleted entity state.
- Empty current sets are first-class and no longer indistinguishable from no-op calls.
- Existing stale rows are repaired automatically from authoritative state.
- Store and backfill behavior share one durable reconciliation contract.

### Negative

- Each write for a keyed entity performs one reverse-index delete before inserting
  its current key rows.
- The first deployment performs one complete repair pass for each already-watermarked
  keyed type.

### Risks

- A reconciliation implementation that deletes before validating new claims could
  release a valid old key on a rejected write. Stores therefore validate first and
  keep all mutations in the same transaction/lock.
- Trusting the index during the repair would make an unremoved stale or missing row
  authoritative. The versioned watermark withholds authoritative misses until the
  pass succeeds.

### DST Compliance

- The actor derives rows only from the transition table and post-event state.
- The sim store continues to use its deterministic lock and ordered collections;
  no clock, randomness, ambient I/O, or thread is added.
- Seeded tests cover faults, retry, restart, and ownership reclaim with `sim_now()`
  and `sim_uuid()`.

## Non-Goals

- Making Turso's declared-key index authoritative.
- Adding index maintenance to composite-action `append_batch` (tracked by ARN-205).
- Changing declared-key hashing or OData key-resolution semantics.
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
