# ARN-462 — Spec

Kernel contract for the three production storms that starve ordinary reads.
One PR on `nerdsane/temper`.

## A. Empty exact-match list reconcile

When `$filter` is a pure equality conjunction and the native page is empty,
the planner may distrust the page (ARN-89 projection lag). That reconcile
MUST consider only:

- the declared-key candidate set (0 or 1 id), and/or
- the field-index coverage gap (ids of this type with no `entity_field_index`
  row for a filtered field), unioned with a re-run native page.

It MUST NOT pass every id of the type to `read_from_source_cursor` merely
because the type is under `scan_candidate_budget`. The production failure is
`materialized_count = 1275`, `returned_count = 0`, reason
`projection_lag_reconcile`.

A just-committed entity that is journal-durable but unprojected remains
findable (it is in the gap). A genuine miss returns bounded empty. A backend
that cannot probe field-index coverage keeps today's over-budget 413; it must
not invent a full-type hydrate to compensate when the type is in budget.

## B. Passivate per tick

`passivate_idle_actors` snapshots then stops idle actors (ADR-0048). One
invocation MUST process at most `PASSIVATE_IDLE_ACTORS_PER_TICK` actors
(units: actors / tick). Remainder stay registered and idle for the next
tick. Actors that are processed still get a snapshot attempt with the
existing retry policy before stop.

## C. Add-rule persistence

`POST /api/tenants/{tenant}/policies/rules` validates the prospective enabled
set, then persists **only the new rule text** under a new `policy_id`.
It does not write the concatenated tenant blob to `primary`. It does not
disable `primary`. If an enabled row already has identical `cedar_text`,
it inserts nothing.

## D. load-inline

Inline `cedar_policies` stay rejected. Verification runs for the submitted
IOA set only (merge already preserves other types). If an unbounded File
walk remains on the success path (ADR warning), it is bounded and must not
hydrate every File actor. No verification-result cache.

## Proof

- Query-plane test: empty equality filter against an in-budget fully-projected
  type does not materialize every id.
- Existing ARN-89 / ARN-68 tests still pass (repair the gap; bound the large type).
- Passivation test: many idle actors are not all snapshotted in one call.
- Policy test: add-rule leaves `primary` text unchanged and does not duplicate
  identical enabled text.
