# Issue 78: Governed Schema Bootstrap Dispatch

Temper planning is unavailable because no Temper MCP server is connected to
this session. This file records the required fallback plan for GitHub issue 78.

## Plan

1. Record the authority, idempotency, partial-commit, and recovery contract in
   ADR-0192 before implementation.
2. Add the bootstrap SDK operation, exact artifact grant, Cedar action, bounded
   request/receipt types, and canonical request digest.
3. Add a durable bootstrap operation record and reserve/progress/complete store
   contract plus an atomic exact-target ownership claim to the simulated,
   PostgreSQL, and Turso schema-deployment stores.
4. Validate entity fields and optional action parameters against the reserved
   bundle's canonical CSDL/IOA closure.
5. Route creation through the normal scoped actor and the optional action
   through an `AgentContext` carrying the exact reserved pin.
6. Resume incomplete reservations after cache eviction and restart using stable
   journal and action idempotency identities; replay completed receipts exactly.
7. Cover grant and Cedar denial, bundle lifecycle drift, closure mismatch,
   conflicting keys, cross-key/cross-caller same-target races, partial outcomes,
   budgets, concurrency, injected faults, cache eviction, and cold restart end
   to end.
8. Run mandatory DST and code-quality reviews, focused and workspace checks,
   live local E2E, merge/deploy, and Datadog verification.

## Acceptance Criteria

- A tenant-global workflow activates a scoped bundle and bootstraps its first
  entity without `/tdata` or schema-scope request headers.
- Host authority plus the durable active pointer is the only source of tenant,
  caller, scope, bundle, and pin identity; the request selects the pointer only
  through its opaque original activation receipt identity.
- The entity and optional action use the exact reserved pin and survive restart.
- Same-key retries return the exact receipt without duplicate creation/action;
  request-digest conflicts fail closed.
- Different-key or different-caller races for one exact target admit one durable
  owner; non-owners cannot adopt the winner's journal as a replay.
- Stale, retired, predecessor, mismatched, unverified, cross-tenant, and
  cross-scope attempts are rejected.
- A dedicated grant and Cedar action reject missing authority and lookalike
  module identities.
- Exact canonical CSDL/IOA validation rejects absent entity types, fields,
  actions, and parameters.
- Creation remains committed when a later action rejects, and the receipt
  reports the exact partial outcome.
- Budget, schema, authorization, guard, conflict, persistence, and recovery
  failures retain bounded structured classifications.
- Existing tenant-global and scoped typed module-data behavior is unchanged.

## Implementation Status

- ADR-0192 was committed first and amended to bind requests to an opaque
  activation receipt identity plus an atomic exact-target ownership claim.
- The SDK operation, entity-bound dedicated grant, bootstrap receipt, durable
  coordinator contract, Sim/PostgreSQL/Turso stores, exact-closure validation,
  scoped actor bridge, action dispatch, and paginated full-journal-budget
  recovery scan are implemented.
- Sim fault/race coverage, Turso close/reopen coverage, PostgreSQL contract
  coverage, and encoded server E2E coverage pass. The PostgreSQL runtime test
  is compiled but skips locally because `DATABASE_URL` is unavailable.
- The E2E proves exact-pin creation, successful initial action and canonical
  result, guard-rejected partial outcome, dedicated/entity-bound grant denial,
  Cedar denial, pre-creation claim release, concurrent convergence, fresh-host
  exact receipt replay, and partial-receipt replay.
- Successful actions co-commit exact post-action outcome evidence. Rejections
  persist a fenced coordinator checkpoint. Fault tests cover both receipt crash
  windows, including later actor state changes that would alter re-evaluation.
- Strict server clippy, focused Sim/Turso/server tests, the complete server
  library suite, and `cargo test --workspace` pass. The workspace run exposed
  and verified the required migration-lineage boundary update for PostgreSQL
  migration 0014.
- Mandatory DST and code-quality reviews passed with no findings, including
  final re-review of the migration-boundary adjustment. Commit/push and
  deployment handoff remain.
