# ADR-0155: Durable deferred transition effects

- Status: Proposed
- Date: 2026-07-11
- Deciders: Temper core maintainers
- Related:
  - ADR-0048: Dispatch retry and idempotency
  - ADR-0049: State-timeout arming
  - ADR-0056: Inline integration persistence and callback handling
  - ADR-0142: Dispatch acknowledges after projection
  - `crates/temper-server/src/entity_actor/types.rs`
  - `crates/temper-server/src/state/dispatch/effects.rs`
  - `crates/temper-server/src/state/dispatch/cross_entity.rs`

## Context

ARN-188 found that successful transitions can emit deferred child effects:
scheduled actions and child spawn requests. The normal post-dispatch path
runs those effects after the entity actor commits and replies. The
`await_integration = true` inline integration path could return the integration
callback response before running the original transition's deferred effects,
which silently dropped scheduled timers and child spawns.

The smaller control-flow fix dispatches those effects before the inline
callback return. That closes the observed branch bug, but it is still not a
complete durability boundary: after the parent transition commits, a process
crash before post-dispatch can lose the deferred effects unless the caller
retries with the same idempotency key.

## Decision

### Sub-Decision 1: Persist deferred effects on the transition event

`EntityEvent` records the `scheduled_actions` and `spawn_requests` emitted by
the transition. These fields have serde defaults, so older events remain
readable.

**Why this approach**: The parent transition event is already the durable fact
that caused the deferred work. Storing the effect intents beside that fact keeps
the immediate fix small and avoids a parallel store schema before the complete
outbox executor exists.

### Sub-Decision 2: Idempotent retries re-surface the original deferred effects

When an actor sees a duplicate idempotency key, it returns the committed
response shape from current state and, when the original event is still in the
bounded recent-event tail, includes the original scheduled/spawn effects. This
lets a caller retry after a crash-before-response and re-enter post-dispatch
without silently dropping the child work.

**Why this approach**: ADR-0048 already defines caller idempotency as the
dispatch retry contract. Reusing that contract is the smallest safe repair for
the crash window this PR can own.

### Sub-Decision 3: Deferred child work receives deterministic idempotency keys

Scheduled actions and child initial actions derive idempotency keys from:

- tenant
- parent entity type and ID
- parent event sequence number
- effect kind and effect index
- target action or child identity

**Why this approach**: If a retry re-surfaces the same deferred effects, the
target actor can deduplicate the resulting scheduled action or child initial
action instead of executing it twice.

### Sub-Decision 4: Inline integration early returns use the same dispatch helper

The inline-integration callback return path and the normal post-dispatch path
both call the shared deferred-effect dispatcher for the original transition
response.

**Why this approach**: A single helper keeps the "run spawn + schedule" contract
visible and prevents future early-return branches from reintroducing the same
drop.

## Rollout Plan

1. **Phase 0 (Immediate)** — Persist scheduled/spawn effects on `EntityEvent`,
   re-surface them on duplicate idempotency retries, add deterministic
   child-effect idempotency keys, and keep the inline integration regression
   test.
2. **Phase 1 (Follow-up)** — Add a leased durable outbox/ack executor that
   scans unacknowledged effect intents after restart, retries with backoff, and
   records completion.
3. **Phase 2** — Extend the outbox model to all post-dispatch work that must be
   recovered independently of caller retry, including integration callbacks if
   they need stronger than best-effort semantics.

## Readiness Gates

- Inline integration regression proves scheduled actions and child spawns are
  not dropped on the callback early-return path.
- Duplicate idempotency responses preserve deferred effects when the originating
  event is still in the actor's recent event tail.
- Deferred child actions carry deterministic idempotency keys.
- Follow-up outbox gate is required before claiming restart-autonomous exactly
  once delivery for deferred effects.

## Consequences

### Positive

- The original transition event now contains the deferred effect intents it
  produced.
- Caller retry after commit-before-response can re-run post-dispatch without
  losing the transition's child work.
- Duplicate post-dispatch attempts are bounded by deterministic target-side
  idempotency keys.

### Negative

- Event payloads grow when transitions emit scheduled actions or spawn requests.
- Recovery is still coupled to caller retry until the leased outbox exists.
- The recent-event-tail lookup is best effort; if the original event ages out,
  duplicate replies remain safe but cannot re-surface deferred effects.

### Risks

- A transition with very large deferred-effect lists could increase journal
  payload size. Existing transition budgets should stay bounded; new effect
  kinds must preserve those budgets.
- A target action that ignores idempotency could still observe duplicate
  delivery. Current actor dispatch applies idempotency at the actor boundary.

### DST Compliance

- Deferred idempotency keys are deterministic strings derived from committed
  tenant/entity/sequence/effect coordinates.
- No random or wall-clock value is introduced in simulation-visible state.
- Background `tokio::spawn` dispatch remains production side-effect behavior
  and is already annotated as determinism-ok at the call sites.

## Non-Goals

- This ADR does not claim autonomous restart recovery without caller retry.
- This ADR does not introduce a leased outbox, ack table, or global effect
  executor.
- This ADR does not change custom-effect integration delivery semantics.

## Alternatives Considered

1. **Only dispatch before inline early return** — Rejected as too shallow. It
   fixes the observed branch but leaves the commit-before-post-dispatch crash
   window unaddressed.
2. **Full durable outbox in this PR** — Rejected for this immediate fix because
   it requires a new store schema, claim/lease protocol, backoff policy, and
   restart scanner. That remains the correct production-hardening follow-up.
3. **Recompute effects from the spec during retry** — Rejected because spec
   evolution can change effects after the original transition commits; the
   committed event should carry the effect intents produced at commit time.

## Rollback Policy

The serde-default fields are backward-compatible. If this decision proves
wrong, stop reading the persisted effect fields and revert the post-dispatch
retry behavior; older events and newer events remain readable.
