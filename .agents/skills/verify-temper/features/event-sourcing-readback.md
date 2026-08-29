# Event-sourcing readback

## Sub-features
The primary `EntityActor` path is event-sourced: append events -> apply -> snapshot every N; on restart, load snapshot + replay to reconstruct state. Model in `crates/temper-runtime/src/persistence/mod.rs`; actor in `crates/temper-server/src/entity_actor/actor.rs`.

## How to get to it (user POV)
Entity state is not a mutable row - it is the fold of an append-only event journal. Correctness means the journal replays to exactly the state the read plane serves, and survives a crash.

## Driving it
Every dispatch appends a `PersistenceEnvelope` (monotonic `sequence_nr`, event_type, payload, causation/correlation ids) via `store.append_with_index_rows(...)`, co-committing declared key-index (ADR-0153) and vector-index (ADR-0155) rows atomically. Snapshots save every `TEMPER_SNAPSHOT_INTERVAL` events (default 100). On `pre_start`, `replay_events` loads the latest snapshot then folds events since, re-deriving effects through the transition table (`replay_effects` ignores guards - a committed event's guard already passed - and honors the stored `to_status`).

```bash
# replay-parity: reconstruct authoritative state from the journal and compare to the read projection
curl -sS "http://localhost:3600/observe/projections/replay-parity?entity_type=<Type>&limit=100" \
  -H "Authorization: Bearer $KEY" -H "X-Tenant-Id: default"
# -> {"kind":"query_projection_replay_parity","clean":true,"report":{checked,drifted,missing,errors}}

# crash/replay is exercised deterministically by the suites
cargo test -p temper-server --test dst_persistence
cargo test -p temper-server --test dst_lifecycle    # create->crash->respawn->replay->continue
```

## What proves it
`replay-parity` returns `clean:true` (drifted = missing = errors = 0): the journal-replayed authoritative state equals the projected read state for every checked entity. A restart is the determinism proof - `pre_start` rebuilds identical state from snapshot + events. The `dst_persistence` / `dst_lifecycle` suites prove the crash-replay path across seeds.

## Gotchas
- Three replay policies (`actor.rs`): lenient (normal hydration), strict-snapshot, and strict-full-journal. Identity/authority resolution uses strict-full-journal and ignores snapshots on purpose, so a stale snapshot cannot preserve revoked authority.
- Replay enforces contiguous sequence numbers and `actor_id` binding; a `Deleted` tombstone is terminal (no events after it); PUT/PATCH land as `FieldsReplaced`/`FieldsUpdated` events replayed as replace/merge.
- A journal tail longer than `MAX_EVENTS_SINCE_SNAPSHOT` (10_000) without a snapshot is rejected on replay - snapshots must keep up.
