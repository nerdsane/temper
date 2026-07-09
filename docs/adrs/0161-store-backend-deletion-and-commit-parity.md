# ADR-0161: Uniform deleted-entity listing and non-poisoning Redis appends

- Status: Accepted
- Date: 2026-07-07
- Deciders: Temper core maintainers
- Related:
  - ADR-0154: OData read-surface truthfulness (collection reads must reflect durable truth)
  - `crates/temper-server/src/state/entity_ops.rs` (shared cold-path index population)
  - `crates/temper-store-turso/src/store/event_store.rs` (Turso `list_entity_ids`)
  - `crates/temper-store-redis/src/event_store.rs` (Redis `append`)
  - Linear ARN-192 (findings `xc-stores-1`, `stores-other-1`, `xc-stores-2`)

## Context

The four `EventStore` backends implemented the same two operations with different
semantics.

**(a) Deleted-entity visibility.** `list_entity_ids` (whole-tenant) filtered
tombstoned entities only on Turso (`AND NOT EXISTS (... event_type='Deleted')`).
Postgres, Redis, and Sim returned every entity that ever had an event, including
deleted ones. The non-eager startup path `populate_index_from_store` inserts every
returned id into the entity index and marks each observed type hydrated, so a later
`list_entity_ids_lazy` serves straight from the index. Result: after a restart a
deleted entity reappears in `GET /EntitySet` on Postgres/Redis/Sim but not on Turso.
The eager path (`hydrate_from_store`) did not have this bug — it materializes each
entity through `ensure_entity_loaded`, which reads the journal tail and drops
tombstoned entities. So the fix is to give the non-eager path the same deletion
semantics the eager path already has. The Turso SQL predicate was also narrower than
the app's canonical `is_deleted_envelope`, which additionally treats
`payload.action == "Deleted"` as a tombstone.

**(b) Redis reported failure for a committed write.** Redis `append` commits the
events and advances `seq_key` atomically inside a Lua script, then performs
non-atomic post-commit segment bookkeeping (GET/GET/SET/SET, each `?`-propagating).
A transient Redis error in that tail returned `Err` even though the event was
durably committed and the sequence had advanced. The actor kept its stale
`sequence_nr`, so every later append used the wrong `expected` and hit a permanent
`ConcurrencyViolation` (wedged actor); a retry re-applied the command and duplicated
events. Postgres does the equivalent bookkeeping inside one transaction, so it rolls
back atomically — the implementations diverged. The Redis segment metadata is
write-only: nothing outside the store ever reads it back.

## Decision

### Sub-Decision 1: Deletion filtering lives once, in the shared index-population path

`populate_index_from_store` and `populate_index_from_store_by_type` now exclude an
entity from the entity index when its latest persisted event satisfies
`is_deleted_envelope` (the same predicate the hot-path `ensure_entity_loaded` uses).
The check reads the entity's journal tail via the backend-neutral `read_events`
trait method, so all four backends get identical semantics regardless of their SQL,
and the complete predicate (`event_type == "Deleted"` **or**
`payload.action == "Deleted"`) applies everywhere. A type is still marked hydrated
for every type observed in the scan (even one whose entities are all deleted), so the
first lazy list never re-scans.

The Turso-specific `AND NOT EXISTS (... event_type='Deleted')` subquery in
whole-tenant `list_entity_ids` is dropped — the server layer is now the single,
authoritative place that decides deletion, and keeping a second, narrower predicate
in one backend's SQL is exactly the divergence this ADR removes.

**Why this approach**: One predicate, one place, all backends. It also correctly
handles resurrection (an entity re-created after deletion has a non-tombstone last
event, so a pure `EXISTS Deleted` SQL filter would wrongly keep hiding it).

### Sub-Decision 2: A committed Redis append never fails on metadata

Redis `append` returns `Ok(new_seq)` as soon as the Lua script reports the commit.
The post-commit segment bookkeeping is removed: it was never read back, and letting
a non-essential metadata write turn a durably-committed append into an `Err` is what
wedged the actor. This matches Postgres, where the append is atomic with respect to
its own bookkeeping.

**Why drop rather than move into Lua**: the segment records are write-only for the
Redis backend (no reader anywhere in the tree), so moving them into the script would
only preserve dead data at the cost of a more complex, timestamp-in-Lua script.
Less code, same observable behavior.

## Consequences

### Positive
- `GET /EntitySet` no longer resurrects deleted entities on any backend after restart.
- A transient Redis hiccup can no longer wedge an actor or duplicate events.
- Deletion semantics are defined once and are complete (payload-action aware).

### Negative / Tradeoffs
- The non-eager `populate_index_from_store(_by_type)` now reads each candidate's
  journal tail to decide deletion, where it previously did a single list query. This
  is a cold-path cost (bootstrap, or first lazy list of a type), and it mirrors what
  the eager `hydrate_from_store` path already does. It currently uses `read_events`,
  which pulls the full journal per entity; a bounded "read last event"
  (`ORDER BY sequence_nr DESC LIMIT 1` / `LINDEX -1`) primitive is the obvious
  follow-up if bootstrap latency on very large tenants becomes a concern.
- Turso whole-tenant `list_entity_ids` now returns tombstoned pairs at the store
  layer; the server layer filters them. Turso's `list_entity_ids_by_type` and
  `list_entity_ids_limited` keep their own SQL deletion filter (out of scope here)
  and remain correct; the server-side filter is the uniform guarantee that also
  covers Redis/Sim.

### Side effects on other whole-tenant `list_entity_ids` callers
Dropping Turso's whole-tenant SQL filter changes two other (non-`GET /EntitySet`)
consumers of that method, both toward *more* correct behavior — Postgres already
returned tombstoned pairs, so these paths already handled them on that backend:

- `crates/temper-cli/src/migrate_turso_to_postgres.rs` — the Turso→Postgres migration
  previously **silently skipped deleted entities' journals** on Turso; it now migrates
  their full history (Created + Deleted). A migration should preserve durable history,
  so this is a beneficial fix, not a regression.
- `crates/temper-server/src/state/projection_backfill/replay_parity.rs` — replay-parity
  verification now enumerates tombstoned entities on Turso too. Benign: replay
  reconstructs the same deleted/absent state the projection has.

### Risks
- If a `read_events` call fails transiently during population, the entity is kept
  (visible) rather than hidden — the conservative choice for a listing (never hide
  live data on a read error). A subsequent populate corrects a genuinely-deleted
  entity that slipped through.

### Testing
- The `(a)` guarantee has a deterministic, always-on proof on the Sim backend
  (`deleted_entity_index_parity::…_sim`) plus a Turso end-to-end proof
  (`ensure_entity_loaded`, unchanged and still green with the subquery dropped). The
  Redis `(a)` parity test and the `(b)` regression test
  (`committed_append_survives_broken_segment_metadata`) require a live Redis and skip
  silently when `REDIS_URL` is unset — they must run in the Redis CI lane to enforce
  the `(b)` guarantee, which has no Redis-less coverage.

### DST Compliance
- Changes in `temper-server` (simulation-visible) use `read_events` through the
  existing store trait; no new time/random/threading. `SimEventStore` exercises the
  new filter deterministically. No `// determinism-ok` annotations needed.

## Non-Goals
- Reworking the `list_entity_ids_by_type` / `list_entity_ids_limited` SQL deletion
  filters on Turso/Postgres. These remain, so Turso is internally asymmetric (raw
  whole-tenant list, filtered by-type/limited) and `list_entity_ids_limited` with
  `entity_type = None` keeps the same store-layer divergence class this ADR removes
  for the whole-tenant path (Turso filters, Sim/Redis do not). That residual is
  confined to bounded internal tools (replay-parity, migration) and never reaches
  `GET /EntitySet`, which is served entirely through the now-uniform server index.
  Collapsing all store list methods to "return raw, server decides deletion" is the
  clean end state but is deliberately deferred: it would touch every backend's
  by-type/limited SQL and their store-level tests, beyond this bug's scope.
- Materialization-layer deletion guards (the index is the authority here).
- Removing Redis snapshot-history write-only metadata (unrelated to this bug).

## Alternatives Considered
1. **Push a deletion filter into every backend's list SQL (Design B).** Keeps the
   single-query efficiency but re-implements the predicate four times (Redis has no
   server-side JOIN, so it would need per-member reads or SADD/SREM pruning), can't
   express `payload.action` in plain SQL, and mishandles resurrection. Rejected: it
   is the divergence this ADR removes.
2. **Move Redis segment bookkeeping into the Lua script.** Preserves write-only data
   nothing reads, at the cost of a more complex script. Rejected in favor of removal.
