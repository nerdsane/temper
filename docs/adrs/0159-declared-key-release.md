# ADR-0159: Declared-Key Release on Delete

## Status

Accepted (2026-07-12)

## Context

The ADR-0153 `entity_key_index` co-commit derives an entity's key rows from
its post-transition fields on every journal append. A tombstoning write
(`Deleted`) still has the key values in its fields, so the co-commit
re-claimed them: the dead entity kept owning its declared keys (ARN-238).
Keyed reads resolved to a tombstone, and — the durable damage — any new
entity claiming the same key value was rejected with a uniqueness violation
forever, because the uniqueness pre-check found the dead holder. Vectors
already handled this (`index_vectors = status != "Deleted"` plus
delete-then-insert reconcile); keys had no equivalent.

## Decision

1. **Release markers.** `EntityKeyRow.key_hash == ""` now means RELEASE: the
   store drops the entity's existing row for that `key_name` and inserts
   nothing, in the same transaction as the journal append. `persist_event`
   emits one row per declared key on every keyed write — a real hash for
   living entities with resolvable keys, a release marker when the write
   tombstones the entity (`state.status == "Deleted"` or
   `event.to_status == "Deleted"` — the Delete arm persists before mutating
   status) or when the key's components are all null/absent. This also
   releases ownership when a key is fully nulled, the issue's second leg.
2. **Stores skip markers everywhere they claim.** The uniqueness pre-check
   and the insert skip empty hashes; the per-key-name delete always runs.
   Applied to the postgres and sim stores (Turso does not maintain the key
   index live; its reads route through the same store impls).
3. **Backfill heals legacy stale rows — both legs.** Tombstoned entities
   emit release markers for every declared key, and LIVING entities emit a
   release marker for any declared key that no longer resolves (nulled
   components) alongside real hashes for the keys that do — computed before
   the already-keyed resume shortcut, since a stale row is exactly what
   makes such an entity look already-keyed. `EntityLoadOutcome` gained a
   `Tombstoned` variant for the delete leg; the already-keyed resume
   shortcut loads the entity before deciding (pre-watermark only), while
   living, fully-resolvable already-keyed entities still skip the write.

## Consequences

- Deleting an entity frees its declared key values for reuse immediately and
  atomically with the tombstone event.
- Legacy stale rows heal on the next boot backfill for types that are not
  yet watermarked. Types already watermarked do not re-run the backfill, so
  their stale rows persist until a manual re-backfill — recorded here
  honestly; the live release prevents any new staleness.
- The backfill's resume path loads already-keyed entities once per run
  (pre-watermark only) instead of fast-skipping on membership; the watermark
  still prevents any post-completion cost.
- **Composite-write residual:** composite sub-writes append through
  `append_batch`, whose `PersistenceAppend` carries no key rows — composites
  never claimed declared keys, and after this change they do not release
  them either. An actor-keyed entity tombstoned via a composite therefore
  still leaves a stale row (healable by the backfill pre-watermark). This is
  a pre-existing ADR-0153 gap on the composite path, recorded here as a
  tracked follow-up (Linear issue to be filed when it reconnects).

## Alternatives Considered

- **`reconcile_keys` flag mirroring `reconcile_vectors`** (delete ALL the
  entity's key rows per write, insert emitted): changes the `EventStore`
  trait signature across every implementor for the same outcome the release
  marker expresses per key, and loses the per-key granularity.
- **Filtering tombstones at read time** (`lookup_by_key` joining status):
  leaves the uniqueness pre-check rejecting new claimants — the durable
  damage — and puts tombstone knowledge in every reader instead of fixing
  ownership at the write.
