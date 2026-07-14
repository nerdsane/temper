# ADR-0172: Declared-Key Release on Delete and Null

- Status: Accepted
- Date: 2026-07-14
- Deciders: Temper core maintainers
- Related:
  - ADR-0153: Declared composite key index
  - ADR-0155: Declared vector access path
  - `crates/temper-server/src/entity_actor/actor.rs`
  - `crates/temper-server/src/state/projection_backfill/key_index.rs`
  - `crates/temper-store-postgres/src/store.rs`
  - `crates/temper-store-sim/src/lib.rs`
  - Linear: ARN-238

## Context

The ADR-0153 `entity_key_index` co-commit derives an entity's key rows from
its post-transition fields on every journal append. A tombstoning write
(`Deleted`) still has the key values in its fields, so the co-commit
re-claimed them: the dead entity kept owning its declared keys (ARN-238).
Keyed reads resolved to a tombstone, and — the durable damage — any new
entity claiming the same key value was rejected with a uniqueness violation
forever, because the uniqueness pre-check found the dead holder.

The same failure mode applies when every component of a declared key becomes
null or absent: the actor emits no row for that key, and the store only
deletes prior rows for key names that appear in the emitted set — so the
old hash stays owned by an entity that can no longer resolve it.

Vectors already handled tombstones (`index_vectors = status != "Deleted"`
plus delete-then-insert reconcile). Keys had no equivalent release path.

## Decision

### 1. Release markers

`EntityKeyRow.key_hash == ""` means RELEASE: the store drops the entity's
existing row for that `key_name` and inserts nothing, in the same
transaction as the journal append.

`persist_event` emits one row per declared key on every keyed write:

- a real hash when the entity is living and the key resolves;
- a release marker when the write tombstones the entity
  (`state.status == "Deleted"` or `event.to_status == "Deleted"` — the
  Delete arm persists before mutating status) or when the key's components
  are all null/absent.

### 2. Stores skip markers on claim paths

The uniqueness pre-check and the insert skip empty hashes; the per-key-name
delete always runs. Applied to the postgres and sim stores (Turso does not
maintain the key index live; its reads route through the same store impls
where the index is active).

### 3. Backfill heals legacy stale rows

Tombstoned entities emit release markers for every declared key, and living
entities emit a release marker for any declared key that no longer resolves
(nulled components) alongside real hashes for keys that do — computed
before the already-keyed resume shortcut, since a stale row is exactly what
makes such an entity look already-keyed.

`EntityLoadOutcome` gains a `Tombstoned` variant for the delete leg. The
already-keyed resume shortcut loads the entity before deciding
(pre-watermark only); living, fully-resolvable already-keyed entities still
skip the write.

## Consequences

### Positive

- Deleting an entity frees its declared key values for reuse immediately and
  atomically with the tombstone event.
- Fully nulled keys stop owning their prior hash on the same write path.
- Legacy stale rows heal on the next boot backfill for types that are not
  yet watermarked.

### Negative / residual

- Types already watermarked do not re-run the backfill, so their pre-existing
  stale rows persist until a manual re-backfill. Live release prevents any
  new staleness after this change.
- The backfill's resume path loads already-keyed entities once per run
  (pre-watermark only) instead of fast-skipping on membership; the watermark
  still prevents any post-completion cost.
- **Composite-write residual:** composite sub-writes append through
  `append_batch`, whose `PersistenceAppend` carries no key rows — composites
  never claimed declared keys, and after this change they do not release
  them either. An actor-keyed entity tombstoned via a composite therefore
  still leaves a stale row (healable by the backfill pre-watermark). This is
  a pre-existing ADR-0153 gap on the composite path, tracked as a follow-up.

### DST Compliance

- No new wall-clock, OS random, or non-deterministic collections on the
  write path. Sim and actor paths continue to use `BTreeMap`/`BTreeSet` and
  simulated time. Empty-hash release markers are pure data.

## Non-Goals

- Re-running backfill for already-watermarked types on every boot.
- Extending composite `append_batch` to carry key rows (separate follow-up).
- Filtering tombstones only at read time (would leave uniqueness rejects).

## Alternatives Considered

1. **`reconcile_keys` flag mirroring `reconcile_vectors`** — delete ALL of
   the entity's key rows per write, then insert emitted rows. Changes the
   `EventStore` trait signature across every implementor for the same
   outcome the release marker expresses per key, and loses per-key
   granularity on partial null of one of several declared keys.
2. **Filtering tombstones at read time** (`lookup_by_key` joining status) —
   leaves the uniqueness pre-check rejecting new claimants (the durable
   damage) and spreads tombstone knowledge into every reader instead of
   fixing ownership at the write.

## Rollback Policy

Revert the actor emission, store release handling, and backfill heal. Any
releases already applied leave the index correct (keys free); a rollback
only reintroduces the stale-ownership bug for subsequent deletes/nulls.
