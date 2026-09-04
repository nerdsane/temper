# ARN-462 — Intent

Production Temper is slow enough that ordinary MCP list/get calls starve. This is
not an MCP host-timeout bug. The spans are in the kernel.

## What Rita asked for

A single Temper call can take 30s. Cursor kills `tools/call` at that cap. On
2026-09-03 (temperpaw sha-63db71e7 / temper 43f9379) three kernel paths burned
the same sqlx pool:

1. `GET /tdata/DesignLanguages` 15.64s — materialized 1275 entities, returned 0,
   skip reason `projection_lag_reconcile`.
2. `entity.passivate_idle_actors` 17s and 24.5s — sequential snapshot+stop of
   430 / 735 idle actors.
3. `POST /api/specs/load-inline` 87.4s — 64.7s busy. On that production SHA the
   path also appended Cedar. Live `primary` had grown to hundreds of KB.

Cedar eval of a 651k blob was a contributor and was cleaned the same day. The
pool storm, the full-type hydrate, and the passivate snapshot storm remain.

## What this effort must make true

- An empty exact-match list against a large (but in-budget) type reconciles only
  the coverage gap / keyed candidates. Returning 0 after hydrating the whole
  type is the failure. ARN-89 read-after-write for entities that might actually
  match stays.
- One passivation tick snapshots a bounded number of idle actors. The rest wait
  for the next tick. Snapshots that do run stay correct (ADR-0048).
- Adding a Cedar rule persists that rule as its own policy row. It does not
  rewrite `primary` with the tenant concat. Identical enabled text is not
  inserted twice. `primary` is not disabled here.
- load-inline on current main already rejects inline Cedar. If
  `load_specs_from_directory(merge: true)` still has a class cost (full
  re-verify of already-loaded specs, unbounded ADR walk), fix that class. If
  the remaining cost is verifying the submitted specs only, record that and do
  not invent a cache.
