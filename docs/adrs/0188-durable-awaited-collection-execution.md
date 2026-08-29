# ADR-0188: Durable awaited collection execution

- Status: Accepted
- Date: 2026-08-28
- Deciders: Temper core maintainers
- Related:
  - ADR-0158: Durable observable entity reactions
  - ADR-0181: Verified bounded collection workflows
  - ADR-0187: Activate public collection workflows with ARC import proof
  - Fork issues #83 and #84
  - `crates/temper-server/src/trigger/`
  - `crates/temper-server/src/state/dispatch/`

## Context

A collection member target commits its `Start` action and reaction receipt
before its awaited WASM integration finishes. That receipt proves admission and
source-state commitment, not execution completion or callback acceptance.
Recovery currently has no later durable boundary and can therefore mistake
admission for completion.

The same delivery retains the generic 30-second reaction lease while awaited
WASM may validly run longer. Recovery can reclaim an expired lease while the
original process is still executing, allowing two owners to race completion
and callback settlement. Collection activation must remain disabled until one
protocol supplies both exact completion evidence and renewable fencing.

## Decision

### Collection members bind one direct awaited WASM integration

The verified member action may contain ordinary deterministic effects and
exactly one custom effect. That effect must resolve to one WASM integration
with a static `on_success` action and an optional static `on_failure` action.
Those callbacks may not directly invoke another integration. Dynamic callbacks,
multiple integrations, adapters, and webhooks are rejected for collection
member actions without changing ordinary non-collection actions.

**Why this approach**: one statically closed invocation gives recovery an exact
completion boundary without introducing a general durable integration graph.

### Delivery journals own exact execution evidence

Each collection member delivery persists a versioned logical execution
identity over its tenant, workflow, member, delivery, integration, module
digest, schema/action pin, and declared callbacks. The mutable lease fence and
attempt remain separate from that stable identity.

The private delivery lifecycle distinguishes no execution evidence,
`Executing`, `ExecutionCompleted`, `CallbackAccepted`, and terminal settlement.
Completion evidence stores canonical callback parameters and their digest with
a 128 KiB budget. Payloads remain private and are never emitted through
Observe, metrics, or logs.

Successful WASM requires its success callback to commit before the member can
succeed. Failed WASM remains a failed member even when its optional failure
callback commits successfully; that callback is durable compensation evidence.

**Why this approach**: recovery can replay a completed callback without
rerunning work and can never infer completion from the earlier `Start` receipt.

### Callback acceptance is atomically fenced

The callback target event, delivery callback-acceptance evidence, and current
collection workflow fence commit in one event-store batch. The batch requires
the exact delivery sequence, execution identity, fence, member, workflow epoch,
module/schema pin, and callback action. Its idempotency key is stable across
lease takeover, while the batch fence rejects a late former owner.

**Why this approach**: there is no crash window between callback commitment and
the evidence needed to recover it.

### Execution leases renew only within the workflow deadline

The queue claim remains 30 seconds. Its exact executor renews every 10 seconds
while execution or callback settlement is active. Renewal is a journal append
requiring the current execution identity, fence, and lifecycle state; expiry is
`min(now + 30 seconds, workflow deadline)`. The integration timeout bounds the
WASM invocation further but never extends the workflow deadline.

Renewal loss, storage failure, cancellation, timeout, deadline, process loss,
or revoked authority stops the executor from completing, calling back, or
settling. After expiry, recovery raises the fence. It preserves and replays
durable completion evidence, but reruns WASM when execution was unresolved.

**Why this approach**: a short failure-detection lease stays independent of the
bounded application deadline and cannot become an unbounded heartbeat.

## Rollout Plan

1. Add the private evidence reader/writer and restrictive bundle validation
   while collection mode remains disabled.
2. Add renewal, takeover, callback fencing, deterministic fault coverage, and
   redacted telemetry to draft PR #69.
3. Leave activation and the authentic ARC proof to issue #39.

## Readiness Gates

- Sim, Turso, and PostgreSQL lifecycle behavior agrees.
- Delayed execution, process loss, stale completion, callback replay,
  cancellation, and timeout races pass deterministic fault tests.
- Mandatory DST and code reviews pass without unresolved findings.
- Public collection mode remains disabled until issue #39's activation gates.

## Consequences

### Positive

- Recovery has exact evidence for every awaited execution boundary.
- Long valid work retains one owner without weakening failure detection.
- Callback replay does not rerun completed WASM work.

### Negative

- Collection v1 cannot use multiple or nested integrations.
- Completion evidence duplicates bounded callback parameters in a private
  delivery journal until settlement.
- Long execution writes one renewal snapshot every 10 seconds.

### Risks

- A storage outage can prevent renewal even while WASM is healthy. Failing
  closed favors single ownership over availability.
- Dropping an unresolved WASM future cannot make its external host effects
  exactly once. Durable external-operation ambiguity remains a separate
  contract.

### DST Compliance

All lifecycle methods accept explicit scheduler timestamps and use ordered,
bounded data. Production timing only wakes the deterministic renewal command;
simulation advances the same commands explicitly. No lifecycle decision reads
wall clock, environment, filesystem, network, or OS randomness.

## Non-Goals

- Multiple or nested awaited integrations in collection v1.
- Exactly-once external network operations.
- Redis-backed parity for this protocol.
- ARC application changes or the 1,120-task activation proof.

## Alternatives Considered

1. **One delivery-level completion boolean** — rejected because it cannot
   distinguish completed work awaiting callback from accepted callback.
2. **Renew leases without completion evidence** — rejected because recovery
   would still infer completion from `Start`.
3. **A durable integration DAG** — rejected because collection v1 needs one
   statically bound invocation, not a general workflow engine.

## Rollback Policy

Keep collection mode disabled, drain any preview workflows, and revert the
private readers and writers together. Never revert renewal while leaving
collection admission enabled, because that restores competing-owner behavior.
