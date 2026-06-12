# ADR-0142: A Dispatch Is Acknowledged Only After Its Query Projection Lands

## Status

Accepted

## Context

OData point-reads (`/tdata/Entity('id')`) are served from the durable
query-plane projection (`entity_catalog`); only a *missing* row falls back to
the entity actor. The projection row, however, was written by a **spawned
background task** after the action committed — a deliberate choice ("must not
sit on the action-dispatch critical path") pinned by a unit test and tracked
with queue-lag metrics.

That combination is unsound: a dispatch returns 200 with the new state, the
caller reads the entity back, and the read races the background upsert. The
system's own modules read-after-write — `scm_assign_pr_number` dispatches
`PullRequest.AssignNumber` (acknowledged 200), then `github_rest_pulls` reads
the row to render the response. In the Genesis workflow round-trip this lost
the race ~40% of the time (2/5 local runs, 2/2 CI runs on one day): the commit
log showed `AssignNumber … succeeded` 7 ms *before* the response rendered
`"number": 0`. Retrying reads in callers would be a band-aid in every consumer
of an API whose acknowledgement lies.

## Decision

`run_post_dispatch_effects` applies the query-projection upsert (or remove)
**inline** and only then returns the dispatch response
(`apply_query_projection_update`, mode `Inline`). An acknowledged action is
immediately readable.

Safety with concurrent writers is preserved by the store guard: projection
writes carry the entity `sequence_nr` and the SQL refuses to regress a row
(`sequence_nr <= new`), so in-flight background writers (boot backfill,
on-read repair, reconcilers — which stay asynchronous) cannot overwrite a
newer inline row.

## Consequences

- **Read-your-writes on the successful path for every dispatch caller** —
  HTTP, OData, WASM integrations, composite reactions. The Genesis workflow
  round-trip went from ~40% flake to stable (5/5 local) with this change.
- **Residual: a projection-write *failure* still acknowledges the dispatch.**
  The action is committed and non-idempotent, so failing the response would
  lie in the other direction; the row stays stale until shadow/parity repair
  (stale rows are not repaired on read — only missing ones are). This is
  logged at `error` and counted by `record_update_error`. Whether a dispatch
  should instead surface a degraded acknowledgement on projection failure is
  an open tradeoff deliberately left for review, not silently decided here.
- **Latency:** each dispatch pays one sequence-guarded row upsert on the same
  store that already persisted the event synchronously, behind the
  `WritePriority::Low` gate (timeout-bounded at 30 s, so worst case under
  contention is a bounded stall, never a hang; gate waits ≥100 ms already
  log). Production impact pending re-measurement by the Genesis
  benchmark (docs/PERFORMANCE.md there) after this ships — pre-change,
  REST PR open/merge had ~2× headroom over github.com.
- **Amends ADR-0082 Sub-Decision 3:** the dispatch-path metrics source label
  changes `background_dispatch` → `inline_dispatch`, and
  `record_update_queue_wait` now measures ~0 for this source (enqueue and
  start are the same task). Dashboards or monitors filtered on
  `source:background_dispatch` must be updated.
- If a workload ever needs the background mode back, it must come with a
  read path that does not serve acknowledged-stale rows (e.g. actor
  read-through), not by reintroducing the race.
- Trajectory persistence (audit log) stays background — nothing reads it
  synchronously.
