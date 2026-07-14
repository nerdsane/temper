# ADR-0173: Vector-Backfill Staleness Guard

## Status

Accepted (2026-07-14)

(Numbered 0173: Claude's concurrent arena branch uses 0161 for the same
decision; Codex's uses 0171 for a heavier monotonic-reconciliation redesign.
This ADR is the Grok arena number and is intentionally unique.)

## Context

The ADR-0155 vector backfill is two store calls with nothing spanning them:
load an entity's current state (snapshot/replay at some journal sequence),
then `backfill_entity_vectors` — an unconditional delete-then-insert
reconcile of the entity's index rows. A live write landing between the two
co-commits the new embedding, and the reconcile then overwrites it with rows
parsed from the stale load (ARN-216). The backfill then stamps the
completion watermark, so reads treat the stale index as authoritative until
the entity's next write. The same shape exists on the Turso write-behind
path, where a retried lagging index write can land after a newer one.

## Decision

1. **`as_of_sequence` on `backfill_entity_vectors`.** Every reconcile
   carries the journal sequence its rows were derived from. Inside the
   store's transaction/lock, the entity's current journal sequence is read;
   if it has advanced past `as_of_sequence`, the reconcile is skipped — the
   newer write's co-commit (or write-behind) already holds newer rows.
   Callers that know their rows are current pass `u64::MAX`.
2. **The loader carries the sequence.** `EntityLoadOutcome::Fields` and
   `Skip` now carry the replayed `sequence_nr`, so both the index-write arm
   and the tombstone/phantom purge arm pass the true as-of sequence.
3. **Turso write-behind passes its append's sequence**, so a retried lagging
   write cannot clobber a subsequently appended newer one.
4. **Skipped-as-stale counts as success for the watermark.** A skip means a
   newer live write reconciled the entity's rows itself; the entity's index
   entry is current by construction.

## Consequences

- The interleave is pinned by a 100-seed DST executing the exact two-step
  production order (`dst_vector_backfill_must_not_overwrite_newer_live_write`).
- The guard costs one indexed `MAX(sequence_nr)` lookup inside the existing
  reconcile transaction — only on backfill/write-behind paths, never on the
  co-commit fast path.
- The declared-key backfill has the same load-then-write shape, and in the
  race interleave the stale upsert lands last and CLOBBERS the newer live
  mapping for that key_name — the same corruption shape as vectors, and it
  remains unguarded pending the symmetric follow-up (Linear issue on
  reconnect) rather than being silently expanded into this change.
- Guard-skip-as-success assumes every journal-advancing writer holds the
  type's vector declarations (`reconcile_vectors` derives from its own
  table). A writer instance without the deployed vector config (rolling
  deploy, stale table) advances the journal without reconciling; a
  guard-skip then trusts rows that write never installed. Pre-existing,
  shared with the key index, recorded here for the record.
- On postgres the guard runs DELETE-first and re-checks the journal, rolling
  back when it advanced — READ COMMITTED makes a check-then-delete ordering
  non-atomic there; sim (mutex) and turso (Immediate transaction) are atomic
  with either ordering. The DELETE's row locks serialize concurrent
  reconciles ONLY when prior rows exist; in the no-prior-rows case a
  same-model race is still caught by the index primary key (the stale INSERT
  collides, errors, and the type is not watermarked — fail-safe), but a
  CROSS-model live re-embed racing the window between the re-check and the
  stale INSERT commits under a different primary key: a transient stale row
  survives in the old model partition until the entity's next write
  reconciles all its partitions. Narrow, self-healing, and strictly better
  than the unguarded behavior — recorded as a known residual alongside the
  key-backfill follow-up rather than closed with a heavier per-entity lock
  (the direction Alternatives rejects).

## Alternatives Considered

- **Locking the entity across load + reconcile:** spans an actor replay and
  a store transaction across two subsystems; far more machinery for the
  same guarantee the sequence comparison gives atomically inside the
  existing transaction.
- **Re-loading and diffing before write:** still racy (TOCTOU between the
  re-load and the write); the guard must live inside the store transaction.
- **Monotonic sequence column on every vector row with CAS upserts:** richer
  and closer to a full projection-contract rewrite (Codex ADR-0171 direction);
  deferred pending ARN-201's canonical append/projection transaction contract.
  The as-of guard is the minimal fix for the permanent-stale-watermark failure.
