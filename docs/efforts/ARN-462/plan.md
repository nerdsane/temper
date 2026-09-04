# ARN-462 — Plan

## What we are addressing

Production lists and passivation storm the sqlx pool; add-rule rewrites
`primary` into a growing Cedar blob; load-inline can sit on the CPU for tens
of seconds. Ordinary MCP calls starve.

## Expected end state

On this branch, with tests:

- Empty exact-match reconcile hydrates gap / keyed candidates only.
- Passivation is capped per tick; leftover idle actors wait.
- Add-rule writes a new policy row (or no-ops on identical enabled text).
- load-inline ADR File walk is bounded; remaining verify cost is documented
  if it is inherent to the submitted specs.

Not claimed: production is fixed (this is not deployed).

## Steps

1. Worktree `cursor-arn462-list-passivate` off origin/main `ff0774f5`.
2. Write `docs/efforts/ARN-462/{intent,spec,plan,decisions}.md`.
3. Red tests, then the four fixes:
   - query-plane: run the existing gap-reconcile for in-budget empty exact-match
     as well as over-budget.
   - `passivate_idle_actors`: oldest-idle first, cap `PASSIVATE_IDLE_ACTORS_PER_TICK`.
   - `handle_add_policy_rule`: persist the rule under `rule:{sha256}`, not `primary`.
   - ADR walk: catalog Path first, explicit file-scan budget; no verify cache.
4. Run the new tests plus query-plane, `passivation_respawn`, and policy suites.
5. Commit explicit paths; push; draft PR as rita-aga if tests pass.

## Deferred / out of scope

- Deploy / live prod verification.
- Shrinking the already-cleaned live Cedar blob.
- Changing PUT `/policies` (full replace of `primary` stays a replace).
- A verification-result cache.
